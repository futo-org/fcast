import { BrowserWindow, ipcMain, IpcMainEvent, nativeImage, Tray, Menu, dialog, shell } from 'electron';
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
import { AddressInfo } from 'ws';
import { v4 as uuidv4 } from 'uuid';

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
    static proxyServer: http.Server;
    static proxyServerAddress: AddressInfo;
    static proxiedFiles: Map<string, { url: string, headers: { [key: string]: string } }> = new Map();

    private static async updateNotify() {
        const upateURL = 'https://github.com/futo-org/fcast/releases';
        const updatePrompt = await dialog.showMessageBox({
            type: 'info',
            title: 'Major update available',
            message: 'Please visit https://fcast.org/ to download the latest update',
            buttons: ['Download', 'Later'],
            defaultId: 0
        });

        if (updatePrompt.response === 0) {
            shell.openExternal(upateURL);
        }
    }

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
                    await Main.updateNotify();
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
            l.emitter.on("play", async (message) => {
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

        Main.updateNotify();
    }


    private static setupProxyServer(): Promise<void> {
        return new Promise<void>((resolve, reject) => {
            try {
                console.log(`Proxy server starting`);

                const port = 0;
                Main.proxyServer = http.createServer((req, res) => {
                    console.log(`Request received`);
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
                        console.error(`Problem with request: ${e.message}`);
                        res.writeHead(500);
                        res.end();
                    });
                });
                Main.proxyServer.on('error', e => {
                    reject(e);
                });
                Main.proxyServer.listen(port, '127.0.0.1', () => {
                    Main.proxyServerAddress = Main.proxyServer.address() as AddressInfo;
                    console.log(`Proxy server running at http://127.0.0.1:${Main.proxyServerAddress.port}/`);
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
        console.log("Proxied url", { proxiedUrl, url, headers });
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