import { BrowserWindow, ipcMain, IpcMainEvent, nativeImage, Tray, Menu, dialog } from 'electron';
import path = require('path');
import { TcpListenerService } from './TcpListenerService';
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from './Packets';
import { DiscoveryService } from './DiscoveryService';
import { Updater } from './Updater';
import { WebSocketListenerService } from './WebSocketListenerService';
import * as os from 'os';
import { Opcode } from './FCastSession';

export default class Main {
    static shouldOpenMainWindow = true; 
    static playerWindow: Electron.BrowserWindow;
    static mainWindow: Electron.BrowserWindow;
    static application: Electron.App;
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static tray: Tray;
    static key: string = null;
    static cert: string = null;

    private static createTray() {
        const icon = (process.platform === 'win32') ? path.join(__dirname, 'app.ico') : path.join(__dirname, 'app.png');
        const trayicon = nativeImage.createFromPath(icon)
        const tray = new Tray(trayicon.resize({ width: 16 }));
        const contextMenu = Menu.buildFromTemplate([
            {
                label: 'Open window',
                click: () => Main.openMainWindow()
            },
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

    private static onReady() {
        Main.createTray();

        Main.discoveryService = new DiscoveryService();
        Main.discoveryService.start();
        
        Main.tcpListenerService = new TcpListenerService();
        Main.webSocketListenerService = new WebSocketListenerService();

        const listeners = [Main.tcpListenerService, Main.webSocketListenerService];
        listeners.forEach(l => {
            l.emitter.on("play", (message) => {
                if (Main.playerWindow == null) {
                    Main.playerWindow = new BrowserWindow({
                        fullscreen: true,
                        autoHideMenuBar: true,
                        webPreferences: {
                            preload: path.join(__dirname, 'player/preload.js')
                        }
                    });
    
                    Main.playerWindow.setAlwaysOnTop(false, 'pop-up-menu');
                    Main.playerWindow.show();
            
                    Main.playerWindow.loadFile(path.join(__dirname, 'player/index.html'));
                    Main.playerWindow.on('ready-to-show', () => {
                        Main.playerWindow?.webContents?.send("play", message);
                    });
                    Main.playerWindow.on('closed', () => {
                        Main.playerWindow = null;
                    });
                } else {
                    Main.playerWindow?.webContents?.send("play", message);
                }            
            });
            
            l.emitter.on("pause", () => Main.playerWindow?.webContents?.send("pause"));
            l.emitter.on("resume", () => Main.playerWindow?.webContents?.send("resume"));
    
            l.emitter.on("stop", () => {
                Main.playerWindow.close();
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
    }

    static getAllIPv4Addresses() {
        const interfaces = os.networkInterfaces();
        const ipv4Addresses: string[] = [];
    
        for (const interfaceName in interfaces) {
            const addresses = interfaces[interfaceName];
            if (!addresses) continue;
    
            for (const addressInfo of addresses) {
                if (addressInfo.family === 'IPv4' && !addressInfo.internal) {
                    ipv4Addresses.push(addressInfo.address);
                }
            }
        }
    
        return ipv4Addresses;
    }

    static openMainWindow() {
        if (Main.mainWindow) {
            Main.mainWindow.focus();
            return;
        }

        Main.mainWindow = new BrowserWindow({
            fullscreen: true,
            autoHideMenuBar: true,
            minWidth: 500,
            minHeight: 920,
            webPreferences: {
                preload: path.join(__dirname, 'main/preload.js')
            }
        });

        Main.mainWindow.loadFile(path.join(__dirname, 'main/index.html'));
        Main.mainWindow.on('closed', () => {
            Main.mainWindow = null;
        });

        Main.mainWindow.show();

        Main.mainWindow.on('ready-to-show', () => {
            Main.mainWindow.webContents.send("device-info", {name: os.hostname(), addresses: Main.getAllIPv4Addresses()});
        });
    }    

    static main(app: Electron.App) {
        Main.application = app;
        const argv = process.argv;
        if (argv.includes('--no-main-window')) {
            Main.shouldOpenMainWindow = false;
        }

        Main.application.on('ready', Main.onReady);
        Main.application.on('window-all-closed', () => { });
    }
}