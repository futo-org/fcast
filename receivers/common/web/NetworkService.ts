import { PlayMessage } from 'common/Packets';
import { streamingMediaTypes } from 'common/MimeTypes';
import { MediaCache } from './MediaCache';
import { http, https } from 'modules/follow-redirects';
import * as url from 'url';
import { v4 as uuidv4 } from 'modules/uuid';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('NetworkService', LoggerType.BACKEND);

export class NetworkService {
    static key: string = null;
    static cert: string = null;
    static proxyServer: http.Server;
    static proxyServerAddress;
    static proxiedFiles: Map<string, PlayMessage> = new Map();

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

                    if (proxyInfo.url.startsWith('app://')) {
                        let start: number = 0;
                        let end: number = null;
                        const contentSize = MediaCache.getInstance().getObjectSize(proxyInfo.url);
                        if (req.headers.range) {
                            const range = req.headers.range.slice(6).split('-');
                            start = (range.length > 0) ? parseInt(range[0]) : 0;
                            end = (range.length > 1) ? parseInt(range[1]) : null;
                        }

                        logger.debug(`Fetching byte range from cache: start=${start}, end=${end}`);
                        const stream = MediaCache.getInstance().getObject(proxyInfo.url, start, end);
                        let responseCode = null;
                        let responseHeaders = null;

                        if (start != 0) {
                            responseCode = 206;
                            responseHeaders = {
                                'Accept-Ranges': 'bytes',
                                'Content-Length': contentSize - start,
                                'Content-Range': `bytes ${start}-${end ? end : contentSize - 1}/${contentSize}`,
                                'Content-Type': proxyInfo.container,
                            };
                        }
                        else {
                            responseCode = 200;
                            responseHeaders = {
                                'Accept-Ranges': 'bytes',
                                'Content-Length': contentSize,
                                'Content-Type': proxyInfo.container,
                            };
                        }

                        logger.debug(`Serving content ${proxyInfo.url} with response headers:`, responseHeaders);
                        res.writeHead(responseCode, responseHeaders);
                        stream.pipe(res);
                    }
                    else {
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

                        const protocol = proxyInfo.url.startsWith('https') ? https : http;
                        const parsedUrl = url.parse(proxyInfo.url);
                        const options: http.RequestOptions | https.RequestOptions = {
                            ... parsedUrl,
                            method: req.method,
                            headers: { ...filteredHeaders, ...proxyInfo.headers }
                        };

                        const proxyReq = protocol.request(options, (proxyRes) => {
                            res.writeHead(proxyRes.statusCode, proxyRes.headers);
                            proxyRes.pipe(res, { end: true });
                        });

                        req.pipe(proxyReq, { end: true });
                        proxyReq.on('error', (e) => {
                            logger.error(`Problem with request: ${e.message}`);
                            res.writeHead(500);
                            res.end();
                        });
                    }
                });
                NetworkService.proxyServer.on('error', e => {
                    reject(e);
                });
                NetworkService.proxyServer.listen(port, '127.0.0.1', () => {
                    NetworkService.proxyServerAddress = NetworkService.proxyServer.address();
                    logger.info(`Proxy server running at http://127.0.0.1:${NetworkService.proxyServerAddress.port}/`);
                    resolve();
                });
            } catch (e) {
                reject(e);
            }
        });
    }

    static async proxyPlayIfRequired(message: PlayMessage): Promise<string> {
        if (message.url && (message.url.startsWith('app://') || (message.headers && !streamingMediaTypes.find(v => v === message.container.toLocaleLowerCase())))) {
            return await NetworkService.proxyFile(message);
        }
        return null;
    }

    static async proxyFile(message: PlayMessage): Promise<string> {
        if (!NetworkService.proxyServer) {
            await NetworkService.setupProxyServer();
        }

        const proxiedUrl = `http://127.0.0.1:${NetworkService.proxyServerAddress.port}/${uuidv4()}`;
        logger.info("Proxied url", { proxiedUrl, message });
        NetworkService.proxiedFiles.set(proxiedUrl, message);
        return proxiedUrl;
    }
}
