import { BrowserWindow, ipcMain, IpcMainEvent, nativeImage, Tray, Menu, dialog } from 'electron';
import path = require('path');
import { FCastService } from './FCastService';
import { PlaybackUpdateMessage, SetVolumeMessage, VolumeUpdateMessage } from './Packets';
import { DiscoveryService } from './DiscoveryService';
import { Updater } from './Updater';

export default class Main {
    static mainWindow: Electron.BrowserWindow;
    static application: Electron.App;
    static service: FCastService;
    static discoveryService: DiscoveryService;
    static tray: Tray;

    private static createTray() {
        const icon = (process.platform === 'win32') ? path.join(__dirname, 'app.ico') : path.join(__dirname, 'app.png');
        const trayicon = nativeImage.createFromPath(icon)
        const tray = new Tray(trayicon.resize({ width: 16 }));
        const contextMenu = Menu.buildFromTemplate([
            {
                label: 'Check for updates',
                click: async () => {
                    try {
                        const updater = new Updater(path.join(__dirname, '../'), 'https://releases.grayjay.app/fcastreceiver');
                        if (await updater.update()) {
                            const restartPrompt = await dialog.showMessageBox({
                                type: 'info',
                                title: 'Update completed',
                                message: 'The application has been updated. Restart now to apply the changes.',
                                buttons: ['Restart'],
                                defaultId: 0
                            });

                            console.log('Update completed');
                        
                            // Restart the app if the user clicks the 'Restart' button
                            if (restartPrompt.response === 0) {
                                Main.application.relaunch();
                                Main.application.exit(0);
                            }
                        } else {
                            await dialog.showMessageBox({
                                type: 'info',
                                title: 'Already up-to-date',
                                message: 'The application is already on the latest version.',
                                buttons: ['OK'],
                                defaultId: 0
                            });
                        }
                    } catch (err) {
                        await dialog.showMessageBox({
                            type: 'error',
                            title: 'Failed to update',
                            message: 'The application failed to update.',
                            buttons: ['OK'],
                            defaultId: 0
                        });

                        console.error('Failed to update:', err);
                    }
                },
            },
            {
                type: 'separator',
            },
            {
                label: 'Restart',
                click: () => {
                    this.application.relaunch();
                    this.application.exit(0);
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
        this.tray = tray;
    }
    
    private static onClose() {
        Main.mainWindow = null;
    }

    private static onReady() {
        Main.createTray();

        Main.discoveryService = new DiscoveryService();
        Main.discoveryService.start();
        
        Main.service = new FCastService();
        Main.service.emitter.on("play", (message) => {
            if (Main.mainWindow == null) {
                Main.mainWindow = new BrowserWindow({
                    fullscreen: true,
                    autoHideMenuBar: true,
                    webPreferences: {
                        preload: path.join(__dirname, 'preload.js')
                    }
                });

                Main.mainWindow.setAlwaysOnTop(false, 'pop-up-menu');
                Main.mainWindow.show();
        
                Main.mainWindow.loadFile(path.join(__dirname, 'index.html'));
                Main.mainWindow.on('ready-to-show', () => {
                    Main.mainWindow?.webContents?.send("play", message);
                });
                Main.mainWindow.on('closed', Main.onClose);
            } else {
                Main.mainWindow?.webContents?.send("play", message);
            }            
        });
        
        Main.service.emitter.on("pause", () => Main.mainWindow?.webContents?.send("pause"));
        Main.service.emitter.on("resume", () => Main.mainWindow?.webContents?.send("resume"));

        Main.service.emitter.on("stop", () => {
            Main.mainWindow.close();
            Main.mainWindow = null;
        });

        Main.service.emitter.on("seek", (message) => Main.mainWindow?.webContents?.send("seek", message));
        Main.service.emitter.on("setvolume", (message) => Main.mainWindow?.webContents?.send("setvolume", message));
        Main.service.start();

        ipcMain.on('toggle-full-screen', () => {
            const window = Main.mainWindow;
            if (!window) {
                return;
            }

            window.setFullScreen(!window.isFullScreen());
        });

        ipcMain.on('exit-full-screen', () => {
            const window = Main.mainWindow;
            if (!window) {
                return;
            }

            window.setFullScreen(false);
        });

        ipcMain.on('send-playback-update', (event: IpcMainEvent, value: PlaybackUpdateMessage) => {
            Main.service.sendPlaybackUpdate(value);
        });

        ipcMain.on('send-volume-update', (event: IpcMainEvent, value: VolumeUpdateMessage) => {
            Main.service.sendVolumeUpdate(value);
        });
    }

    static main(app: Electron.App) {
        Main.application = app;
        Main.application.on('ready', Main.onReady);
        Main.application.on('window-all-closed', () => { });
    }
}