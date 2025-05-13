import { PlayMessage } from 'common/Packets';
import { streamingMediaTypes } from 'common/MimeTypes';
import * as http from 'http';
import * as url from 'url';
import { AddressInfo } from 'modules/ws';
import { v4 as uuidv4 } from 'modules/uuid';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('NetworkService', LoggerType.BACKEND);

export class NetworkService {
    static key: string = null;
    static cert: string = null;
    static proxyServer: http.Server;
    static proxyServerAddress: AddressInfo;
    static proxiedFiles: Map<string, { url: string, headers: { [key: string]: string } }> = new Map();

    private static setupProxyServer(): Promise<void> {
        return new Promise<void>((resolve, reject) => {
            try {
                logger.info(`Proxy server starting`);

                const port = 0;
                NetworkService.proxyServer = http.createServer((req, res) => {
                    logger.info(`Request received`);
                    const requestUrl = `http://${req.headers.host}${req.url}`;

                    const proxyInfo = NetworkService.proxiedFiles.get(requestUrl);

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
                        logger.error(`Problem with request: ${e.message}`);
                        res.writeHead(500);
                        res.end();
                    });
                });
                NetworkService.proxyServer.on('error', e => {
                    reject(e);
                });
                NetworkService.proxyServer.listen(port, '127.0.0.1', () => {
                    NetworkService.proxyServerAddress = NetworkService.proxyServer.address() as AddressInfo;
                    logger.info(`Proxy server running at http://127.0.0.1:${NetworkService.proxyServerAddress.port}/`);
                    resolve();
                });
            } catch (e) {
                reject(e);
            }
        });
    }

    static async proxyPlayIfRequired(message: PlayMessage): Promise<PlayMessage> {
        if (message.headers && message.url && !streamingMediaTypes.find(v => v === message.container.toLocaleLowerCase())) {
            return { ...message, url: await NetworkService.proxyFile(message.url, message.headers) };
        }
        return message;
    }

    static async proxyFile(url: string, headers: { [key: string]: string }): Promise<string> {
        if (!NetworkService.proxyServer) {
            await NetworkService.setupProxyServer();
        }

        const proxiedUrl = `http://127.0.0.1:${NetworkService.proxyServerAddress.port}/${uuidv4()}`;
        logger.info("Proxied url", { proxiedUrl, url, headers });
        NetworkService.proxiedFiles.set(proxiedUrl, { url: url, headers: headers });
        return proxiedUrl;
    }
}
