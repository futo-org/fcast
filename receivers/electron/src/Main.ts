/* eslint-disable @typescript-eslint/ban-ts-comment */
import { BrowserWindow, ipcMain, IpcMainEvent, nativeImage, Tray, Menu, dialog, shell, MenuItemConstructorOptions, MenuItem } from 'electron';
import { ToastIcon } from 'common/components/Toast';
import { Opcode, PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage, PlayMessage, PlayUpdateMessage, EventMessage, EventType, PlaylistContent, SeekMessage, SetVolumeMessage, SetSpeedMessage, SetPlaylistItemMessage, MetadataType, GenericMediaMetadata } from 'common/Packets';
import { DiscoveryService } from 'common/DiscoveryService';
import { TcpListenerService } from 'common/TcpListenerService';
import { ConnectionMonitor } from 'common/ConnectionMonitor';
import { Logger, LoggerType } from 'common/Logger';
import { MediaCache } from 'common/MediaCache';
import { supportedImageExtensions, supportedVideoExtensions } from 'common/MimeTypes';
import { Settings } from 'common/Settings';
import { preparePlayMessage } from 'common/UtilityBackend';
import { Updater } from './Updater';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import yargs from 'yargs';
import { hideBin } from 'yargs/helpers';
const cp = require('child_process');
let logger = null;

const APPLICATION_TITLE = 'FCast Receiver';

class AppCache {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    public interfaces: any[] = null;
    public appName: string = null;
    public appVersion: string = null;
    public playMessage: PlayMessage = null;
    public playerVolume: number = null;
    public playbackUpdate: PlaybackUpdateMessage = null;
    public subscribedKeys = new Set<string>();
}

export class Main {
    static shouldOpenMainWindow = true;
    static startFullscreen = false;
    static playerWindow: Electron.BrowserWindow;
    static mainWindow: Electron.BrowserWindow;
    static application: Electron.App;
    static tcpListenerService: TcpListenerService;
    static discoveryService: DiscoveryService;
    static connectionMonitor: ConnectionMonitor;
    static tray: Tray;
    static cache: AppCache = new AppCache();
    static noFullscreenPlayer = false;
    static noPlayerWindow = false;
    static password: string | null;

    private static playerWindowContentViewer = null;
    private static mediaCache: MediaCache = null;

    private static toggleMainWindow() {
        if (Main.mainWindow) {
            Main.mainWindow.close();
        }
        else {
            Main.openMainWindow();
        }
    }

    private static async checkForUpdates(silent: boolean) {
        // @ts-ignore
        if (UPDATER) {
            if (Updater.updateDownloaded) {
                Main.mainWindow?.webContents?.send("download-complete");
                return;
            }

            try {
                const updateAvailable = await Updater.checkForUpdates();

                if (updateAvailable) {
                    Main.mainWindow?.webContents?.send("update-available");
                }
                else {
                    if (!silent) {
                        await dialog.showMessageBox({
                            type: 'info',
                            title: 'Already up-to-date',
                            message: 'The application is already on the latest version.',
                            buttons: ['OK'],
                            defaultId: 0
                        });
                    }
                }
            } catch (err) {
                if (!silent) {
                    await dialog.showMessageBox({
                        type: 'error',
                        title: 'Failed to check for updates',
                        message: err,
                        buttons: ['OK'],
                        defaultId: 0
                    });
                }

                logger.error('Failed to check for updates:', err);
            }
        }
    }

    private static createTray() {
        const icon = (process.platform === 'win32') ? path.join(__dirname, 'assets/icons/app/icon.ico') : path.join(__dirname, 'assets/icons/app/icon.png');
        const trayicon = nativeImage.createFromPath(icon)
        const tray = new Tray(trayicon.resize({ width: 16 }));
        const menuItems: (MenuItemConstructorOptions | MenuItem)[] = [
            {
                label: 'Toggle window',
                click: () => { Main.toggleMainWindow(); }
            },
            {
                label: 'Check for updates',
                click: async () => { await Main.checkForUpdates(false); },
            },
            {
                label: 'About',
                click: async () => {
                    let aboutMessage = `Version: ${Main.application.getVersion()}\n`;

                    // @ts-ignore
                    if (UPDATER) {
                        if (Updater.getCommit()) {
                            aboutMessage += `Commit: ${Updater.getCommit()}\n`;
                        }

                        aboutMessage += `Release channel: ${Updater.releaseChannel}\n`;

                        if (Updater.releaseChannel !== 'stable') {
                            aboutMessage += `Release channel version: ${Updater.getChannelVersion()}\n`;
                        }
                    }

                    aboutMessage += `OS: ${process.platform} ${process.arch}\n`;

                    await dialog.showMessageBox({
                        type: 'info',
                        title: APPLICATION_TITLE,
                        message: aboutMessage,
                        buttons: ['OK'],
                        defaultId: 0
                    });
                },
            },
            {
                type: 'separator',
            },
            {
                label: 'Restart',
                click: () => {
                    this.application.relaunch();
                    this.application.exit();
                }
            },
            {
                label: 'Quit',
                click: () => {
                    this.application.quit();
                }
            }
        ];

        // @ts-ignore
        if (!UPDATER) {
            menuItems.splice(1, 1);
        }

        const contextMenu = Menu.buildFromTemplate(menuItems);
        tray.setContextMenu(contextMenu);

        // Left-click opens up tray menu, unlike in Windows/Linux
        if (process.platform !== 'darwin') {
            tray.on('click', () => { Main.toggleMainWindow(); } );
        }

        this.tray = tray;
    }

    private static async play(message: PlayMessage) {
        Main.tcpListenerService.send(Opcode.PlayUpdate, new PlayUpdateMessage(Date.now(), message));
        Main.cache.playMessage = message;
        const messageInfo = await preparePlayMessage(message, Main.cache.playerVolume, (playMessage: PlaylistContent) => {
            Main.mediaCache?.destroy();
            Main.mediaCache = new MediaCache(playMessage);
        });

        let windowTitle = APPLICATION_TITLE;
        if (message.metadata?.type === MetadataType.Generic) {
            const metadata = message.metadata as GenericMediaMetadata;
            windowTitle = metadata.title ? `${metadata.title} - ${APPLICATION_TITLE}` : APPLICATION_TITLE;
        }

        if (!Main.playerWindow) {
            if (!Main.noPlayerWindow) {
                Main.playerWindow = new BrowserWindow({
                    fullscreen: !Main.noFullscreenPlayer,
                    autoHideMenuBar: true,
                    icon: path.join(__dirname, 'icon512.png'),
                    title: windowTitle,
                    webPreferences: {
                        preload: path.join(__dirname, 'player/preload.js')
                    }
                });

                Main.playerWindow.setAlwaysOnTop(false, 'pop-up-menu');
                Main.playerWindow.show();

                Main.playerWindow.loadFile(path.join(__dirname, `${messageInfo.contentViewer}/index.html`));
                Main.playerWindow.once('ready-to-show', async () => {
                    Main.playerWindow?.webContents?.send(messageInfo.rendererEvent, messageInfo.rendererMessage);
                });
                Main.playerWindow.on('closed', () => {
                    Main.playerWindow = null;
                    Main.playerWindowContentViewer = null;
                });
            } else {
                if (!Main.mainWindow) {
                    Main.openMainWindow(messageInfo);
                } else {
                    Main.playerWindow = Main.mainWindow;
                    Main.mainWindow.loadFile(path.join(__dirname, `${messageInfo.contentViewer}/index.html`));
                    Main.mainWindow.once('ready-to-show', async () => {
                        Main.mainWindow?.webContents?.send(messageInfo.rendererEvent, messageInfo.rendererMessage);
                    });
                }
            }
        }
        else if (Main.playerWindow && messageInfo.contentViewer !== Main.playerWindowContentViewer) {
            Main.playerWindow.setTitle(windowTitle);
            Main.playerWindow.loadFile(path.join(__dirname, `${messageInfo.contentViewer}/index.html`));
            Main.playerWindow.once('ready-to-show', async () => {
                Main.playerWindow?.webContents?.send(messageInfo.rendererEvent, messageInfo.rendererMessage);
            });
        } else {
            Main.playerWindow.setTitle(windowTitle);
            Main.playerWindow?.webContents?.send(messageInfo.rendererEvent, messageInfo.rendererMessage);
        }

        Main.playerWindowContentViewer = messageInfo.contentViewer;
    }

    private static onReady() {
        Main.createTray();

        Main.connectionMonitor = new ConnectionMonitor();
        Main.discoveryService = new DiscoveryService();
        Main.discoveryService.start();

        Main.tcpListenerService = new TcpListenerService();
        Main.tcpListenerService.emitter.on("play", (message: PlayMessage) => Main.play(message));
        Main.tcpListenerService.emitter.on("pause", () => Main.playerWindow?.webContents?.send("pause"));
        Main.tcpListenerService.emitter.on("resume", () => Main.playerWindow?.webContents?.send("resume"));

        Main.tcpListenerService.emitter.on("stop", () => {
            if (!Main.noPlayerWindow) {
                Main.playerWindow?.close();
            } else {
                if (Main.mainWindow?.webContents.getURL().lastIndexOf('main/index.html') == -1) {
                    Main.mainWindow?.loadFile(path.join(__dirname, 'main/index.html'));
                }
            }

            Main.playerWindow = null;
            Main.playerWindowContentViewer = null;
            Main.cache.playbackUpdate = null;
            Main.cache.playMessage = null;
        });

        Main.tcpListenerService.emitter.on("seek", (message: SeekMessage) => Main.playerWindow?.webContents?.send("seek", message));
        Main.tcpListenerService.emitter.on("setvolume", (message: SetVolumeMessage) => {
            Main.cache.playerVolume = message.volume;
            Main.playerWindow?.webContents?.send("setvolume", message);
        });
        Main.tcpListenerService.emitter.on("setspeed", (message: SetSpeedMessage) => Main.playerWindow?.webContents?.send("setspeed", message));

        Main.tcpListenerService.emitter.on('connect', (message) => {
            ConnectionMonitor.onConnect(Main.tcpListenerService, message, () => {
                Main.sendWindowEvent('connect', message);
            });
        });
        Main.tcpListenerService.emitter.on('disconnect', (message) => {
            ConnectionMonitor.onDisconnect(message, () => {
                Main.sendWindowEvent('disconnect', message);
            });
        });
        Main.tcpListenerService.emitter.on('ping', (sessionId: string) => {
            ConnectionMonitor.onPingPong(sessionId);
        });
        Main.tcpListenerService.emitter.on('pong', (sessionId: string) => {
            ConnectionMonitor.onPingPong(sessionId);
        });
        Main.tcpListenerService.emitter.on('initial', (message) => {
            logger.info(`Received 'Initial' message from sender:`, message);
        });
        Main.tcpListenerService.emitter.on("setplaylistitem", (message: SetPlaylistItemMessage) => Main.playerWindow?.webContents?.send("setplaylistitem", message));
        Main.tcpListenerService.emitter.on('subscribeevent', (message) => {
            Main.tcpListenerService.subscribeEvent(message.sessionId, message.body.event);

            if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                const subscribeData = {
                    keyDown: new Set([...tcpListenerSubscribedKeys.keyDown]),
                    keyUp: new Set([...tcpListenerSubscribedKeys.keyUp]),
                };

                Main.sendWindowEvent("event-subscribed-keys-update", subscribeData);
            }
        });
        Main.tcpListenerService.emitter.on('unsubscribeevent', (message) => {
            Main.tcpListenerService.unsubscribeEvent(message.sessionId, message.body.event);

            if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                const subscribeData = {
                    keyDown: new Set([...tcpListenerSubscribedKeys.keyDown]),
                    keyUp: new Set([...tcpListenerSubscribedKeys.keyUp]),
                };

                Main.sendWindowEvent("event-subscribed-keys-update", subscribeData);
            }
        });
        Main.tcpListenerService.start(Main.password);

        ipcMain.on('send-playback-error', (event: IpcMainEvent, value: PlaybackErrorMessage) => {
            Main.tcpListenerService.send(Opcode.PlaybackError, value);
        });

        ipcMain.on('send-playback-update', (event: IpcMainEvent, value: PlaybackUpdateMessage) => {
            Main.cache.playbackUpdate = value;
            Main.tcpListenerService.send(Opcode.PlaybackUpdate, value);
        });

        ipcMain.on('send-volume-update', (event: IpcMainEvent, value: VolumeUpdateMessage) => {
            Main.cache.playerVolume = value.volume;
            Main.tcpListenerService.send(Opcode.VolumeUpdate, value);
        });

        ipcMain.on('send-event', (event: IpcMainEvent, value: EventMessage) => {
            Main.tcpListenerService.send(Opcode.Event, value);
        });

        ipcMain.on('play-request', (event: IpcMainEvent, value: PlayMessage, playlistIndex: number) => {
            logger.debug(`Received play request for index ${playlistIndex}:`, value);
            value.url = Main.mediaCache?.has(playlistIndex) ? Main.mediaCache?.getUrl(playlistIndex) : value.url;
            Main.mediaCache?.cacheItems(playlistIndex);
            Main.play(value);
        });

        // @ts-ignore
        if (UPDATER) {
            ipcMain.on('send-download-request', async () => {
                if (!Updater.isDownloading) {
                    try {
                        await Updater.downloadUpdate();
                        Main.mainWindow?.webContents?.send("download-complete");
                    } catch (err) {
                        await dialog.showMessageBox({
                            type: 'error',
                            title: 'Failed to download update',
                            message: err,
                            buttons: ['OK'],
                            defaultId: 0
                        });

                        logger.error('Failed to download update:', err);
                        Main.mainWindow?.webContents?.send("download-failed");
                    }
                }
            });

            ipcMain.on('send-restart-request', async () => {
                Updater.restart();
            });

            ipcMain.handle('updater-progress', async () => { return Updater.updateProgress; });
        }

        ipcMain.handle('is-full-screen', async () => {
            const window = Main.playerWindow;
            if (!window) {
                return;
            }

            return window.isFullScreen();
        });

        ipcMain.on('toggle-full-screen', () => {
            const window = Main.playerWindow;
            if (!window) {
                return;
            }

            window.setFullScreen(!window.isFullScreen());
        });

        ipcMain.on('exit-full-screen', () => {
            const window = Main.playerWindow;
            if (!window) {
                return;
            }

            window.setFullScreen(false);
        });

        ipcMain.handle('get-sessions', () => {
            return Main.tcpListenerService.getSenders();
        });

        ipcMain.handle('get-subscribed-keys', () => {
            const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
            const subscribeData = {
                keyDown: new Set([...tcpListenerSubscribedKeys.keyDown]),
                keyUp: new Set([...tcpListenerSubscribedKeys.keyUp]),
            };

            return subscribeData;
        });

        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        ipcMain.on('network-changed', (event: IpcMainEvent, value: any[], wifiSignalUpdate: boolean) => {
            Main.cache.interfaces = value;

            if (!wifiSignalUpdate) {
                Main.discoveryService.stop();
                Main.discoveryService.start();
            }

            Main.mainWindow?.webContents?.send("device-info", { name: os.hostname(), interfaces: value });
        });

        if (Main.shouldOpenMainWindow) {
            Main.openMainWindow();
        }

        // @ts-ignore
        if (UPDATER) {
            if (Updater.updateError) {
                dialog.showMessageBox({
                    type: 'error',
                    title: 'Error applying update',
                    message: 'Please try again later or visit https://fcast.org to update.',
                    buttons: ['OK'],
                    defaultId: 0
                });
            }

            if (Updater.checkForUpdatesOnStart) {
                Main.checkForUpdates(true);
            }
        }

        Main.mainWindow?.webContents?.setWindowOpenHandler((details) => {
            shell.openExternal(details.url);
            return { action: "deny" };
        });
    }

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    private static sendWindowEvent(event: string, data: any) {
        Main.mainWindow?.webContents?.send(event, data);
        if (!Main.noPlayerWindow) {
            Main.playerWindow?.webContents?.send(event, data);
        }
    }

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    static openMainWindow(playMessageInfo: { rendererEvent: string, rendererMessage: any, contentViewer: string } = null) {
        if (Main.mainWindow) {
            Main.mainWindow.focus();
            return;
        }

        Main.mainWindow = new BrowserWindow({
            fullscreen: Main.startFullscreen,
            autoHideMenuBar: true,
            icon: path.join(__dirname, 'icon512.png'),
            webPreferences: {
                preload: path.join(__dirname, 'main/preload.js')
            }
        });

        const networkWorker = new BrowserWindow({
            show: false,
            webPreferences: {
                nodeIntegration: true,
                contextIsolation: false,
                preload: path.join(__dirname, 'main/networkWorker.js')
            }
        });

        if (!Main.noPlayerWindow || !playMessageInfo) {
            Main.mainWindow.loadFile(path.join(__dirname, 'main/index.html'));
        } else if (playMessageInfo) {
            Main.playerWindow = Main.mainWindow;
            Main.mainWindow.loadFile(path.join(__dirname, `${playMessageInfo.contentViewer}/index.html`));
            Main.mainWindow.once('ready-to-show', async () => {
                Main.mainWindow?.webContents?.send(playMessageInfo.rendererEvent, playMessageInfo.rendererMessage);
            });
        }

        Main.mainWindow.on('closed', () => {
            Main.mainWindow = null;

            if (Main.noPlayerWindow) {
                Main.playerWindow = null;
                Main.playerWindowContentViewer = null;
                Main.cache.playbackUpdate = null;
                Main.cache.playMessage = null;
            }

            if (!networkWorker.isDestroyed()) {
                networkWorker.close();
            }
        });

        Main.mainWindow.maximize();
        Main.mainWindow.show();

        Main.mainWindow.on('ready-to-show', () => {
            if (Main.cache.interfaces) {
                Main.mainWindow?.webContents?.send("device-info", { name: os.hostname(), interfaces: Main.cache.interfaces });
            }

            networkWorker.loadFile(path.join(__dirname, 'main/worker.html'));
        });

        Main.mainWindow.webContents.on('dom-ready', () => {
            if (Settings.json.ui.mainWindowBackground !== '') {
                if (fs.existsSync(Settings.json.ui.mainWindowBackground)) {
                    if (supportedVideoExtensions.find(ext => ext === path.extname(Settings.json.ui.mainWindowBackground).toLocaleLowerCase())) {
                        Main.mainWindow?.webContents?.send("update-background", Settings.json.ui.mainWindowBackground, true);
                    }
                    else if (supportedImageExtensions.find(ext => ext === path.extname(Settings.json.ui.mainWindowBackground).toLocaleLowerCase())) {
                        Main.mainWindow?.webContents?.send("update-background", Settings.json.ui.mainWindowBackground, false);
                    }
                    else {
                        logger.warn(`Custom background at ${Settings.json.ui.mainWindowBackground} is not of a supported file format. Ignoring...`);
                    }
                }
                else {
                    logger.warn(`Custom background at ${Settings.json.ui.mainWindowBackground} does not exist. Ignoring...`);
                }
            }
        });
    }

    static async main(app: Electron.App) {
        try {
            Main.application = app;
            Main.cache.appName = "FCast Receiver Electron";
            Main.cache.appVersion = app.getVersion();

            // Using singleton classes for better compatibility running on webOS
            const jsonPath = path.join(app.getPath('userData'), 'UserSettings.json');
            new Settings(jsonPath);

            if (Settings.json.network.ignoreCertificateErrors) {
                app.commandLine.appendSwitch('ignore-certificate-errors');
            }

            const argv = yargs(hideBin(process.argv))
            .version(app.getVersion())
            .parserConfiguration({
                'boolean-negation': false
            })
            .options({
                'no-main-window': { type: 'boolean', desc: "Start minimized to tray" },
                'fullscreen': { type: 'boolean', desc: "Start application in fullscreen" },
                'log': { chocies: ['ALL', 'TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR', 'FATAL', 'MARK', 'OFF'], alias: 'loglevel', desc: "Defines the verbosity level of the logger" },
                'no-fullscreen-player': { type: 'boolean', desc: "Start player in windowed mode" },
                'no-player-window': { type: 'boolean', desc: "Play videos in the main application window" },
                'password': { type: 'string', desc: "Password to expect from incoming commands" },
            })
            .parseSync();

            let isUpdating = false;
            // @ts-ignore
            if (UPDATER) {
                new Updater();
                isUpdating = Updater.isUpdating();
            }

            const fileLogType = isUpdating ? 'fileSync' : 'file';
            Logger.initialize({
                appenders: {
                    out: { type: 'stdout' },
                    log: { type: fileLogType, filename: path.join(app.getPath('logs'), 'fcast-receiver.log'), flags: 'a', maxLogSize: '5M' },
                },
                categories: {
                    default: { appenders: ['out', 'log'], level: argv.log === undefined ? Settings.json.log.level : argv.log },
                },
            });
            logger = new Logger('Main', LoggerType.BACKEND);
            logger.info(`Starting application: ${app.name} | ${app.getAppPath()}`);
            logger.info(`Version: ${app.getVersion()}`);
            logger.info(`OS: ${process.platform} ${process.arch}`);

            // @ts-ignore
            if (UPDATER) {
                logger.info(`Commit: ${Updater.getCommit()}`);
                logger.info(`Release channel: ${Updater.releaseChannel} - ${Updater.getChannelVersion()}`);
            }

            process.setUncaughtExceptionCaptureCallback(async (error) => await errorHandler(error));

            // @ts-ignore
            if (UPDATER) {
                if (isUpdating) {
                    await Updater.processUpdate();
                }
            }

            Main.startFullscreen = argv.fullscreen === undefined ? Settings.json.ui.fullscreen : argv.fullscreen;
            Main.shouldOpenMainWindow = argv.noMainWindow === undefined ? !Settings.json.ui.noMainWindow : !argv.noMainWindow;
            Main.noFullscreenPlayer = argv.noFullscreenPlayer === undefined ? Settings.json.ui.noFullscreenPlayer : argv.noFullscreenPlayer;
            Main.noPlayerWindow = argv.noPlayerWindow === undefined ? Settings.json.ui.noPlayerWindow : argv.noPlayerWindow;
            Main.password = argv.password === undefined ? null : argv.password;

            const lock = Main.application.requestSingleInstanceLock()
            if (!lock) {
                Main.application.quit();
                return;
            }
            Main.application.on('second-instance', () => {
                if (Main.mainWindow) {
                    if (Main.mainWindow.isMinimized()) {
                        Main.mainWindow.restore();
                    }
                    Main.mainWindow.focus();
                }
                else {
                    Main.openMainWindow();
                }
            })

            Main.application.on('certificate-error', (_event, _webContents, url, error, certificate) => {
                toast('Could not playback media (certificate error)', ToastIcon.ERROR);
                logger.error('Could not playback media (certificate error):', { url: url, error: error, certificate: certificate });
            });
            Main.application.on('ready', Main.onReady);
            Main.application.on('window-all-closed', () => { });
        }
        catch (err) {
            logger.error(`Error starting application: ${err}`);
            app.exit();
        }
    }
}

export function toast(message: string, icon: ToastIcon = ToastIcon.INFO, duration: number = 5000) {
    Main.mainWindow?.webContents?.send('toast', message, icon, duration);
    if (!Main.noPlayerWindow) {
        Main.playerWindow?.webContents?.send('toast', message, icon, duration);
    }
}

export function getComputerName() {
    if (Settings.json.network.deviceName !== '') {
        return Settings.json.network.deviceName;
    }

    switch (process.platform) {
        case "win32":
            return `FCast-${process.env.COMPUTERNAME}`;
        case "darwin":
            return `FCast-${cp.execSync("scutil --get ComputerName").toString().trim()}`;
        case "linux": {
            let hostname: string;

            // Some distro's don't work with `os.hostname()`, but work with `hostnamectl` and vice versa...
            try {
                hostname = os.hostname();
            }
            catch (err) {
                logger.warn('Error fetching hostname, trying different method...');
                logger.warn(err);

                try {
                    hostname = cp.execSync("hostnamectl hostname").toString().trim();
                }
                catch (err2) {
                    logger.warn('Error fetching hostname again, using generic name...');
                    logger.warn(err2);

                    hostname = 'linux device';
                }
            }

            return `FCast-${hostname}`;
        }

        default:
            return `FCast-${os.hostname()}`;
    }
}

export function getAppName() {
    return Main.cache.appName;
}

export function getAppVersion() {
    return Main.cache.appVersion;
}

export function getPlayMessage() {
    return Main.cache.playbackUpdate === null ? Main.cache.playMessage : {
        ...Main.cache.playMessage,
        time: Main.cache.playbackUpdate.time,
        volume: Main.cache.playerVolume,
        speed: Main.cache.playbackUpdate.speed
    };
}

export function getPlaybackUpdateMessage() {
    return Main.cache.playbackUpdate;
}

export async function errorHandler(error: Error) {
    if (attemptErrorRecovery(error)) {
        return;
    }

    logger.error(error);
    logger.shutdown();

    const restartPrompt = await dialog.showMessageBox({
        type: 'error',
        title: 'Application Error',
        message: `The application encountered an error:\n\n${error.stack}}`,
        buttons: ['Restart', 'Close'],
        defaultId: 0,
        cancelId: 1
    });

    if (restartPrompt.response === 0) {
        Main.application.relaunch();
        Main.application.exit(0);
    } else {
        Main.application.exit(0);
    }
}

function attemptErrorRecovery(error: Error): boolean {
    let recovered = false;

    switch (error.message) {
        // Error case: no network interfaces are present while starting the program. mDNS setup will fail, so
        // fallback to operating the program without mDNS if network interfaces later come online.
        case 'addMembership ENODEV':
            if (Main.cache.interfaces === null || Main.cache.interfaces?.length === 0) {
                logger.error('Error starting mDNS discovery service, continuing without it...');
                recovered = true;
            }

            break;

        default:
            break;
    }

    return recovered;
}
