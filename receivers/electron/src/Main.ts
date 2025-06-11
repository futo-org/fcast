import { BrowserWindow, ipcMain, IpcMainEvent, nativeImage, Tray, Menu, dialog, shell } from 'electron';
import { Opcode, PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage, PlayMessage, PlayUpdateMessage, EventMessage, EventType, ContentObject, ContentType, PlaylistContent, SeekMessage, SetVolumeMessage, SetSpeedMessage, SetPlaylistItemMessage } from 'common/Packets';
import { supportedPlayerTypes } from 'common/MimeTypes';
import { DiscoveryService } from 'common/DiscoveryService';
import { TcpListenerService } from 'common/TcpListenerService';
import { WebSocketListenerService } from 'common/WebSocketListenerService';
import { NetworkService } from 'common/NetworkService';
import { ConnectionMonitor } from 'common/ConnectionMonitor';
import { Logger, LoggerType } from 'common/Logger';
import { fetchJSON } from 'common/UtilityBackend';
import { MediaCache } from 'common/MediaCache';
import { Updater } from './Updater';
import * as os from 'os';
import * as path from 'path';
import yargs from 'yargs';
import { hideBin } from 'yargs/helpers';
const cp = require('child_process');
let logger = null;

class AppCache {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    public interfaces: any = null;
    public appName: string = null;
    public appVersion: string = null;
    public playMessage: PlayMessage = null;
    public playerVolume: number = null;
    public subscribedKeys = new Set<string>();
}

export class Main {
    static shouldOpenMainWindow = true;
    static startFullscreen = false;
    static playerWindow: Electron.BrowserWindow;
    static mainWindow: Electron.BrowserWindow;
    static application: Electron.App;
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static connectionMonitor: ConnectionMonitor;
    static tray: Tray;
    static cache: AppCache = new AppCache();

    private static playerWindowContentViewer = null;
    private static listeners = [];
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

    private static createTray() {
        const icon = (process.platform === 'win32') ? path.join(__dirname, 'assets/icons/app/icon.ico') : path.join(__dirname, 'assets/icons/app/icon.png');
        const trayicon = nativeImage.createFromPath(icon)
        const tray = new Tray(trayicon.resize({ width: 16 }));
        const contextMenu = Menu.buildFromTemplate([
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

                    if (Updater.getCommit()) {
                        aboutMessage += `Commit: ${Updater.getCommit()}\n`;
                    }

                    aboutMessage += `Release channel: ${Updater.releaseChannel}\n`;

                    if (Updater.releaseChannel !== 'stable') {
                        aboutMessage += `Release channel version: ${Updater.getChannelVersion()}\n`;
                    }

                    aboutMessage += `OS: ${process.platform} ${process.arch}\n`;

                    await dialog.showMessageBox({
                        type: 'info',
                        title: 'Fcast Receiver',
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
        ])

        tray.setContextMenu(contextMenu);

        // Left-click opens up tray menu, unlike in Windows/Linux
        if (process.platform !== 'darwin') {
            tray.on('click', () => { Main.toggleMainWindow(); } );
        }

        this.tray = tray;
    }

    private static async play(message: PlayMessage) {
        Main.listeners.forEach(l => l.send(Opcode.PlayUpdate, new PlayUpdateMessage(Date.now(), message)));
        Main.cache.playMessage = message;

        // Protocol v2 FCast PlayMessage does not contain volume field and could result in the receiver
        // getting out-of-sync with the sender when player windows are closed and re-opened. Volume
        // is cached in the play message when volume is not set in v3 PlayMessage.
        message.volume = message.volume || message.volume === undefined ? Main.cache.playerVolume : message.volume;

        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        let rendererMessage: any = await NetworkService.proxyPlayIfRequired(message);
        let rendererEvent = 'play';
        let contentViewer = supportedPlayerTypes.find(v => v === message.container.toLocaleLowerCase()) ? 'player' : 'viewer';

        if (message.container === 'application/json') {
            const json: ContentObject = message.url ? await fetchJSON(message.url) : JSON.parse(message.content);

            if (json && json.contentType !== undefined) {
                switch (json.contentType) {
                    case ContentType.Playlist: {
                        rendererMessage = json as PlaylistContent;
                        rendererEvent = 'play-playlist';

                        if ((rendererMessage.forwardCache && rendererMessage.forwardCache > 0) || (rendererMessage.backwardCache && rendererMessage.backwardCache > 0)) {
                            Main.mediaCache?.destroy();
                            Main.mediaCache = new MediaCache(rendererMessage);
                        }

                        const offset = rendererMessage.offset ? rendererMessage.offset : 0;
                        contentViewer = supportedPlayerTypes.find(v => v === rendererMessage.items[offset].container.toLocaleLowerCase()) ? 'player' : 'viewer';
                        break;
                    }

                    default:
                        break;
                }
            }
        }

        if (!Main.playerWindow) {
            Main.playerWindow = new BrowserWindow({
                fullscreen: true,
                autoHideMenuBar: true,
                icon: path.join(__dirname, 'icon512.png'),
                webPreferences: {
                    preload: path.join(__dirname, 'player/preload.js')
                }
            });

            Main.playerWindow.setAlwaysOnTop(false, 'pop-up-menu');
            Main.playerWindow.show();

            Main.playerWindow.loadFile(path.join(__dirname, `${contentViewer}/index.html`));
            Main.playerWindow.on('ready-to-show', async () => {
                Main.playerWindow?.webContents?.send(rendererEvent, rendererMessage);
            });
            Main.playerWindow.on('closed', () => {
                Main.playerWindow = null;
                Main.playerWindowContentViewer = null;
            });
        }
        else if (Main.playerWindow && contentViewer !== Main.playerWindowContentViewer) {
            Main.playerWindow.loadFile(path.join(__dirname, `${contentViewer}/index.html`));
            Main.playerWindow.on('ready-to-show', async () => {
                Main.playerWindow?.webContents?.send(rendererEvent, rendererMessage);
            });
        } else {
            Main.playerWindow?.webContents?.send(rendererEvent, rendererMessage);
        }

        Main.playerWindowContentViewer = contentViewer;
    }

    private static onReady() {
        Main.createTray();

        Main.connectionMonitor = new ConnectionMonitor();
        Main.discoveryService = new DiscoveryService();
        Main.discoveryService.start();

        Main.tcpListenerService = new TcpListenerService();
        Main.webSocketListenerService = new WebSocketListenerService();

        Main.listeners = [Main.tcpListenerService, Main.webSocketListenerService];
        Main.listeners.forEach(l => {
            l.emitter.on("play", (message: PlayMessage) => Main.play(message));
            l.emitter.on("pause", () => Main.playerWindow?.webContents?.send("pause"));
            l.emitter.on("resume", () => Main.playerWindow?.webContents?.send("resume"));

            l.emitter.on("stop", () => {
                Main.playerWindow?.close();
                Main.playerWindow = null;
                Main.playerWindowContentViewer = null;
            });

            l.emitter.on("seek", (message: SeekMessage) => Main.playerWindow?.webContents?.send("seek", message));
            l.emitter.on("setvolume", (message: SetVolumeMessage) => {
                Main.cache.playerVolume = message.volume;
                Main.playerWindow?.webContents?.send("setvolume", message);
            });
            l.emitter.on("setspeed", (message: SetSpeedMessage) => Main.playerWindow?.webContents?.send("setspeed", message));

            l.emitter.on('connect', (message) => {
                ConnectionMonitor.onConnect(l, message, l instanceof WebSocketListenerService, () => {
                    Main.mainWindow?.webContents?.send('connect', message);
                    Main.playerWindow?.webContents?.send('connect', message);
                });
            });
            l.emitter.on('disconnect', (message) => {
                ConnectionMonitor.onDisconnect(l, message, l instanceof WebSocketListenerService, () => {
                    Main.mainWindow?.webContents?.send('disconnect', message);
                    Main.playerWindow?.webContents?.send('disconnect', message);
                });
            });
            l.emitter.on('ping', (message) => {
                ConnectionMonitor.onPingPong(message, l instanceof WebSocketListenerService);
            });
            l.emitter.on('pong', (message) => {
                ConnectionMonitor.onPingPong(message, l instanceof WebSocketListenerService);
            });
            l.emitter.on('initial', (message) => {
                logger.info(`Received 'Initial' message from sender: ${message}`);
            });
            l.emitter.on("setplaylistitem", (message: SetPlaylistItemMessage) => Main.playerWindow?.webContents?.send("setplaylistitem", message));
            l.emitter.on('subscribeevent', (message) => {
                const subscribeData = l.subscribeEvent(message.sessionId, message.body.event);

                if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                    Main.mainWindow?.webContents?.send("event-subscribed-keys-update", subscribeData);
                    Main.playerWindow?.webContents?.send("event-subscribed-keys-update", subscribeData);
                }
            });
            l.emitter.on('unsubscribeevent', (message) => {
                const unsubscribeData = l.unsubscribeEvent(message.sessionId, message.body.event);

                if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                    Main.mainWindow?.webContents?.send("event-subscribed-keys-update", unsubscribeData);
                    Main.playerWindow?.webContents?.send("event-subscribed-keys-update", unsubscribeData);
                }
            });
            l.start();

            ipcMain.on('send-playback-error', (event: IpcMainEvent, value: PlaybackErrorMessage) => {
                l.send(Opcode.PlaybackError, value);
            });

            ipcMain.on('send-playback-update', (event: IpcMainEvent, value: PlaybackUpdateMessage) => {
                l.send(Opcode.PlaybackUpdate, value);
            });

            ipcMain.on('send-volume-update', (event: IpcMainEvent, value: VolumeUpdateMessage) => {
                Main.cache.playerVolume = value.volume;
                l.send(Opcode.VolumeUpdate, value);
            });

            ipcMain.on('send-event', (event: IpcMainEvent, value: EventMessage) => {
                l.send(Opcode.Event, value);
            });
        });

        ipcMain.on('play-request', (event: IpcMainEvent, value: PlayMessage, playlistIndex: number) => {
            logger.debug(`Received play request for index ${playlistIndex}:`, value);
            value.url = Main.mediaCache.has(playlistIndex) ? Main.mediaCache.getUrl(playlistIndex) : value.url;
            Main.mediaCache.cacheItems(playlistIndex);
            Main.play(value);
        });
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

        // Having to mix and match session ids and ip addresses until querying websocket remote addresses is fixed
        ipcMain.handle('get-sessions', () => {
            return [].concat(Main.tcpListenerService.getSenders(), Main.webSocketListenerService.getSessions());
        });

        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        ipcMain.on('network-changed', (event: IpcMainEvent, value: any) => {
            Main.cache.interfaces = value;
            Main.mainWindow?.webContents?.send("device-info", { name: os.hostname(), interfaces: value });
        });

        if (Main.shouldOpenMainWindow) {
            Main.openMainWindow();
        }

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

        Main.mainWindow?.webContents?.setWindowOpenHandler((details) => {
            shell.openExternal(details.url);
            return { action: "deny" };
        });
    }

    static openMainWindow() {
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

        Main.mainWindow.loadFile(path.join(__dirname, 'main/index.html'));
        Main.mainWindow.on('closed', () => {
            Main.mainWindow = null;

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
    }

    static async main(app: Electron.App) {
        try {
            Main.application = app;
            Main.cache.appName = app.name;
            Main.cache.appVersion = app.getVersion();

            const argv = yargs(hideBin(process.argv))
            .version(app.getVersion())
            .parserConfiguration({
                'boolean-negation': false
            })
            .options({
                'no-main-window': { type: 'boolean', default: false, desc: "Start minimized to tray" },
                'fullscreen': { type: 'boolean', default: false, desc: "Start application in fullscreen" },
                'log': { chocies: ['ALL', 'TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR', 'FATAL', 'MARK', 'OFF'], alias: 'loglevel', default: 'INFO', desc: "Defines the verbosity level of the logger" },
            })
            .parseSync();

            const isUpdating = Updater.isUpdating();
            const fileLogType = isUpdating ? 'fileSync' : 'file';
            Logger.initialize({
                appenders: {
                    out: { type: 'stdout' },
                    log: { type: fileLogType, filename: path.join(app.getPath('logs'), 'fcast-receiver.log'), flags: 'a', maxLogSize: '5M' },
                },
                categories: {
                    default: { appenders: ['out', 'log'], level: argv.log },
                },
            });
            logger = new Logger('Main', LoggerType.BACKEND);
            logger.info(`Starting application: ${app.name} | ${app.getAppPath()}`);
            logger.info(`Version: ${app.getVersion()}`);
            logger.info(`Commit: ${Updater.getCommit()}`);
            logger.info(`Release channel: ${Updater.releaseChannel} - ${Updater.getChannelVersion()}`);
            logger.info(`OS: ${process.platform} ${process.arch}`);

            process.setUncaughtExceptionCaptureCallback(async (error) => await errorHandler(error));

            if (isUpdating) {
                await Updater.processUpdate();
            }

            Main.startFullscreen = argv.fullscreen;
            Main.shouldOpenMainWindow = !argv.noMainWindow;

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
            Main.application.on('ready', Main.onReady);
            Main.application.on('window-all-closed', () => { });
        }
        catch (err) {
            logger.error(`Error starting application: ${err}`);
            app.exit();
        }
    }
}

export function getComputerName() {
    switch (process.platform) {
        case "win32":
            return process.env.COMPUTERNAME;
        case "darwin":
            return cp.execSync("scutil --get ComputerName").toString().trim();
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

            return hostname;
        }

        default:
            return os.hostname();
    }
}

export function getAppName() {
    return Main.cache.appName;
}

export function getAppVersion() {
    return Main.cache.appVersion;
}

export function getPlayMessage() {
    return Main.cache.playMessage;
}

export async function errorHandler(error: Error) {
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
