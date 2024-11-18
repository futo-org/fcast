import { BrowserWindow, ipcMain, IpcMainEvent, nativeImage, Tray, Menu, dialog } from 'electron';
import { TcpListenerService } from './TcpListenerService';
import { PlayMessage, PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from './Packets';
import { DiscoveryService } from './DiscoveryService';
import { Updater } from './Updater';
import { WebSocketListenerService } from './WebSocketListenerService';
import { Opcode } from './FCastSession';
import * as os from 'os';
import * as path from 'path';
import * as http from 'http';
import * as url from 'url';
import * as log4js from "log4js";
import { AddressInfo } from 'ws';
import { v4 as uuidv4 } from 'uuid';
import yargs from 'yargs';
import { hideBin } from 'yargs/helpers';

export default class Main {
    static shouldOpenMainWindow = true;
    static startFullscreen = false;
    static playerWindow: Electron.BrowserWindow;
    static mainWindow: Electron.BrowserWindow;
    static application: Electron.App;
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static tray: Tray;
    static key: string = null;
    static cert: string = null;
    static proxyServer: http.Server;
    static proxyServerAddress: AddressInfo;
    static proxiedFiles: Map<string, { url: string, headers: { [key: string]: string } }> = new Map();
    static logger: log4js.Logger;

    private static toggleMainWindow() {
        if (Main.mainWindow) {
            Main.mainWindow.close();
        }
        else {
            Main.openMainWindow();
        }
    }

    private static createTray() {
        const icon = (process.platform === 'win32') ? path.join(__dirname, 'icon.ico') : path.join(__dirname, 'icon.png');
        const trayicon = nativeImage.createFromPath(icon)
        const tray = new Tray(trayicon.resize({ width: 16 }));
        const contextMenu = Menu.buildFromTemplate([
            {
                label: 'Toggle window',
                click: () => { Main.toggleMainWindow(); }
            },
            {
                label: 'Check for updates',
                click: async () => {
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
                            title: 'Failed to check for updates',
                            message: err,
                            buttons: ['OK'],
                            defaultId: 0
                        });

                        Main.logger.error('Failed to check for updates:', err);
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
                        Main.playerWindow?.webContents?.send("play", await Main.proxyPlayIfRequired(message));
                    });
                    Main.playerWindow.on('closed', () => {
                        Main.playerWindow = null;
                    });
                } else {
                    Main.playerWindow?.webContents?.send("play", await Main.proxyPlayIfRequired(message));
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
    }


    private static setupProxyServer(): Promise<void> {
        return new Promise<void>((resolve, reject) => {
            try {
                Main.logger.info(`Proxy server starting`);

                const port = 0;
                Main.proxyServer = http.createServer((req, res) => {
                    Main.logger.info(`Request received`);
                    const requestUrl = `http://${req.headers.host}${req.url}`;

                    const proxyInfo = Main.proxiedFiles.get(requestUrl);

                    if (!proxyInfo) {
                        res.writeHead(404);
                        res.end('Not found');
                        return;
                    }

                    const omitHeaders = new Set([
                        'host',
                        'connection',
                        'keep-alive',
                        'proxy-authenticate',
                        'proxy-authorization',
                        'te',
                        'trailers',
                        'transfer-encoding',
                        'upgrade'
                    ]);

                    const filteredHeaders = Object.fromEntries(Object.entries(req.headers)
                        .filter(([key]) => !omitHeaders.has(key.toLowerCase()))
                        .map(([key, value]) => [key, Array.isArray(value) ? value.join(', ') : value]));

                    const parsedUrl = url.parse(proxyInfo.url);
                    const options: http.RequestOptions = {
                        ... parsedUrl,
                        method: req.method,
                        headers: { ...filteredHeaders, ...proxyInfo.headers }
                    };

                    const proxyReq = http.request(options, (proxyRes) => {
                        res.writeHead(proxyRes.statusCode, proxyRes.headers);
                        proxyRes.pipe(res, { end: true });
                    });

                    req.pipe(proxyReq, { end: true });
                    proxyReq.on('error', (e) => {
                        Main.logger.error(`Problem with request: ${e.message}`);
                        res.writeHead(500);
                        res.end();
                    });
                });
                Main.proxyServer.on('error', e => {
                    reject(e);
                });
                Main.proxyServer.listen(port, '127.0.0.1', () => {
                    Main.proxyServerAddress = Main.proxyServer.address() as AddressInfo;
                    Main.logger.info(`Proxy server running at http://127.0.0.1:${Main.proxyServerAddress.port}/`);
                    resolve();
                });
            } catch (e) {
                reject(e);
            }
        });
    }

    static streamingMediaTypes = [
        "application/vnd.apple.mpegurl",
        "application/x-mpegURL",
        "application/dash+xml"
    ];

    static async proxyPlayIfRequired(message: PlayMessage): Promise<PlayMessage> {
        if (message.headers && message.url && !Main.streamingMediaTypes.find(v => v === message.container.toLocaleLowerCase())) {
            return { ...message, url: await Main.proxyFile(message.url, message.headers) };
        }
        return message;
    }

    static async proxyFile(url: string, headers: { [key: string]: string }): Promise<string> {
        if (!Main.proxyServer) {
            await Main.setupProxyServer();
        }

        const proxiedUrl = `http://127.0.0.1:${Main.proxyServerAddress.port}/${uuidv4()}`;
        Main.logger.info("Proxied url", { proxiedUrl, url, headers });
        Main.proxiedFiles.set(proxiedUrl, { url: url, headers: headers });
        return proxiedUrl;
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
            Main.mainWindow.webContents.send("device-info", {name: os.hostname(), addresses: Main.getAllIPv4Addresses()});
        });
    }

    static async main(app: Electron.App) {
        try {
            Main.application = app;
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
            Main.logger.info(`Starting application: ${app.name} (${app.getVersion()} - ${Updater.getChannelVersion()}) | ${app.getAppPath()}`);

            if (isUpdating) {
                await Updater.processUpdate();
            }

            const argv = yargs(hideBin(process.argv))
                .parserConfiguration({
                    'boolean-negation': false
                })
                .options({
                    'no-main-window': { type: 'boolean', default: false, desc: "Start minimized to tray" },
                    'fullscreen': { type: 'boolean', default: false, desc: "Start application in fullscreen" }
                })
                .parseSync();

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
