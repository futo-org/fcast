import { BrowserWindow, ipcMain, IpcMainEvent, nativeImage, Tray, Menu, dialog } from 'electron';
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from 'common/Packets';
import { DiscoveryService } from 'common/DiscoveryService';
import { TcpListenerService } from 'common/TcpListenerService';
import { WebSocketListenerService } from 'common/WebSocketListenerService';
import { NetworkService } from 'common/NetworkService';
import { Opcode } from 'common/FCastSession';
import { Updater } from './Updater';
import * as os from 'os';
import * as path from 'path';
import * as log4js from "log4js";
import yargs from 'yargs';
import { hideBin } from 'yargs/helpers';
const cp = require('child_process');

export class Main {
    static shouldOpenMainWindow = true;
    static startFullscreen = false;
    static playerWindow: Electron.BrowserWindow;
    static mainWindow: Electron.BrowserWindow;
    static application: Electron.App;
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static tray: Tray;
    static logger: log4js.Logger;

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
            Main.mainWindow.webContents.send("download-complete");
            return;
        }

        try {
            const updateAvailable = await Updater.checkForUpdates();

            if (updateAvailable) {
                Main.mainWindow.webContents.send("update-available");
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

            Main.logger.error('Failed to check for updates:', err);
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

    private static onReady() {
        Main.createTray();

        Main.discoveryService = new DiscoveryService();
        Main.discoveryService.start();

        Main.tcpListenerService = new TcpListenerService();
        Main.webSocketListenerService = new WebSocketListenerService();

        const listeners = [Main.tcpListenerService, Main.webSocketListenerService];
        listeners.forEach(l => {
            l.emitter.on("play", async (message) => {
                if (Main.playerWindow == null) {
                    Main.playerWindow = new BrowserWindow({
                        fullscreen: true,
                        autoHideMenuBar: true,
                        minWidth: 515,
                        minHeight: 290,
                        icon: path.join(__dirname, 'icon512.png'),
                        webPreferences: {
                            preload: path.join(__dirname, 'player/preload.js')
                        }
                    });

                    Main.playerWindow.setAlwaysOnTop(false, 'pop-up-menu');
                    Main.playerWindow.show();

                    Main.playerWindow.loadFile(path.join(__dirname, 'player/index.html'));
                    Main.playerWindow.on('ready-to-show', async () => {
                        Main.playerWindow?.webContents?.send("play", await NetworkService.proxyPlayIfRequired(message));
                    });
                    Main.playerWindow.on('closed', () => {
                        Main.playerWindow = null;
                    });
                } else {
                    Main.playerWindow?.webContents?.send("play", await NetworkService.proxyPlayIfRequired(message));
                }
            });

            l.emitter.on("pause", () => Main.playerWindow?.webContents?.send("pause"));
            l.emitter.on("resume", () => Main.playerWindow?.webContents?.send("resume"));

            l.emitter.on("stop", () => {
                    Main.playerWindow?.close();
                    Main.playerWindow = null;
            });

            l.emitter.on("seek", (message) => Main.playerWindow?.webContents?.send("seek", message));
            l.emitter.on("setvolume", (message) => Main.playerWindow?.webContents?.send("setvolume", message));
            l.emitter.on("setspeed", (message) => Main.playerWindow?.webContents?.send("setspeed", message));
            l.start();

            ipcMain.on('send-playback-error', (event: IpcMainEvent, value: PlaybackErrorMessage) => {
                l.send(Opcode.PlaybackError, value);
            });

            ipcMain.on('send-playback-update', (event: IpcMainEvent, value: PlaybackUpdateMessage) => {
                l.send(Opcode.PlaybackUpdate, value);
            });

            ipcMain.on('send-volume-update', (event: IpcMainEvent, value: VolumeUpdateMessage) => {
                l.send(Opcode.VolumeUpdate, value);
            });

            ipcMain.on('send-download-request', async () => {
                if (!Updater.isDownloading) {
                    try {
                        await Updater.downloadUpdate();
                        Main.mainWindow.webContents.send("download-complete");
                    } catch (err) {
                        await dialog.showMessageBox({
                            type: 'error',
                            title: 'Failed to download update',
                            message: err,
                            buttons: ['OK'],
                            defaultId: 0
                        });

                        Main.logger.error('Failed to download update:', err);
                        Main.mainWindow.webContents.send("download-failed");
                    }
                }
            });

            ipcMain.on('send-restart-request', async () => {
                Updater.restart();
            });
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
            minWidth: 1100,
            minHeight: 800,
            webPreferences: {
                preload: path.join(__dirname, 'main/preload.js')
            }
        });

        Main.mainWindow.loadFile(path.join(__dirname, 'main/index.html'));
        Main.mainWindow.on('closed', () => {
            Main.mainWindow = null;
        });

        Main.mainWindow.maximize();
        Main.mainWindow.show();

        Main.mainWindow.on('ready-to-show', () => {
            Main.mainWindow.webContents.send("device-info", {name: os.hostname(), addresses: NetworkService.getAllIPv4Addresses()});
        });
    }

    static async main(app: Electron.App) {
        try {
            Main.application = app;

            const argv = yargs(hideBin(process.argv))
            .version(app.getVersion())
            .parserConfiguration({
                'boolean-negation': false
            })
            .options({
                'no-main-window': { type: 'boolean', default: false, desc: "Start minimized to tray" },
                'fullscreen': { type: 'boolean', default: false, desc: "Start application in fullscreen" }
            })
            .parseSync();

            const isUpdating = Updater.isUpdating();
            const fileLogType = isUpdating ? 'fileSync' : 'file';
            log4js.configure({
                appenders: {
                    out: { type: 'stdout' },
                    log: { type: fileLogType, filename: path.join(app.getPath('logs'), 'fcast-receiver.log'), flags: 'a', maxLogSize: '5M' },
                },
                categories: {
                    default: { appenders: ['out', 'log'], level: 'info' },
                },
            });
            Main.logger = log4js.getLogger();
            Main.logger.info(`Starting application: ${app.name} | ${app.getAppPath()}`);
            Main.logger.info(`Version: ${app.getVersion()}`);
            Main.logger.info(`Commit: ${Updater.getCommit()}`);
            Main.logger.info(`Release channel: ${Updater.releaseChannel} - ${Updater.getChannelVersion()}`);
            Main.logger.info(`OS: ${process.platform} ${process.arch}`);

            if (isUpdating) {
                await Updater.processUpdate();
            }

            Main.startFullscreen = argv.fullscreen;
            Main.shouldOpenMainWindow = !argv.noMainWindow;
            Main.application.on('ready', Main.onReady);
            Main.application.on('window-all-closed', () => { });
        }
        catch (err) {
            Main.logger.error(`Error starting application: ${err}`);
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
                Main.logger.warn('Error fetching hostname, trying different method...');
                Main.logger.warn(err);

                try {
                    hostname = cp.execSync("hostnamectl hostname").toString().trim();
                }
                catch (err2) {
                    Main.logger.warn('Error fetching hostname again, using generic name...');
                    Main.logger.warn(err2);

                    hostname = 'linux device';
                }
            }

            return hostname;
        }

        default:
            return os.hostname();
    }
}

export async function errorHandler(err: NodeJS.ErrnoException) {
    Main.logger.error("Application error:", err);

    const restartPrompt = await dialog.showMessageBox({
        type: 'error',
        title: 'Failed to start',
        message: 'The application failed to start properly.',
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
