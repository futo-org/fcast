import { PlayMessage } from 'common/Packets';
import { streamingMediaTypes } from 'common/MimeTypes';
import { MediaCache } from './MediaCache';
import { http, https } from 'modules/follow-redirects';
import * as url from 'url';
import { v4 as uuidv4 } from 'modules/uuid';
import { Logger, LoggerType } from 'common/Logger';
import { CompanionBroker, CompanionError, isCompanionUrl, parseCompanionUrl, streamCompanionResource } from 'common/v4/Companion';
const logger = new Logger('NetworkService', LoggerType.BACKEND);

export class NetworkService {
    static key: string = null;
    static cert: string = null;
    static proxyServer: http.Server;
    static proxyServerAddress;
    static proxiedFiles: Map<string, PlayMessage> = new Map();
    // Set by the service when v4 is enabled; serves fcomp:// URLs.
    static companion: CompanionBroker = null;

    // Streams a companion resource to the player, honouring a Range request. The bytes are asked
    // from the providing sender one window at a time, as fast as the player reads them.
    private static serveCompanion(req: http.IncomingMessage, res: http.ServerResponse, proxyInfo: PlayMessage) {
        const parsed = parseCompanionUrl(proxyInfo.url);
        const broker = NetworkService.companion;
        if (parsed === null || broker === null) {
            res.writeHead(404);
            res.end();
            return;
        }

        let closed = false;
        let cancel = () => { /* nothing is streaming yet */ };
        res.on('close', () => {
            closed = true;
            cancel();
        });

        broker.getInfo(parsed.providerId, parsed.resourceId).then((info) => {
            if (closed) {
                return;
            }

            let start = 0;
            let end: number = null;
            let partial = false;
            const range = typeof req.headers.range === 'string' ? /^bytes=(\d+)-(\d*)$/.exec(req.headers.range) : null;
            if (range !== null) {
                start = parseInt(range[1], 10);
                end = range[2] !== '' ? parseInt(range[2], 10) : null;
                partial = true;
            }

            const headers: { [key: string]: string | number } = {
                'Accept-Ranges': 'bytes',
                'Content-Type': info.contentType || proxyInfo.container,
            };
            if (info.size !== null) {
                if (start >= info.size || (end !== null && end < start)) {
                    res.writeHead(416, { 'Content-Range': `bytes */${info.size}` });
                    res.end();
                    return;
                }
                end = end === null ? info.size - 1 : Math.min(end, info.size - 1);
                headers['Content-Length'] = end - start + 1;
                if (partial) {
                    headers['Content-Range'] = `bytes ${start}-${end}/${info.size}`;
                }
            } else if (start !== 0) {
                // A range cannot be described without the total size.
                res.writeHead(416);
                res.end();
                return;
            } else {
                partial = false;
            }

            res.writeHead(partial ? 206 : 200, headers);
            if (req.method === 'HEAD') {
                res.end();
                return;
            }

            const stream = streamCompanionResource(broker, parsed.providerId, parsed.resourceId, start, end, (chunk) => {
                return new Promise<void>((resolve) => {
                    if (res.write(chunk)) {
                        resolve();
                    } else {
                        res.once('drain', resolve);
                    }
                });
            });
            cancel = () => stream.cancel();
            stream.done.then(() => res.end(), (e) => {
                logger.warn(`Companion resource ${proxyInfo.url} failed mid-stream: ${e}`);
                res.destroy();
            });
        }, (e) => {
            if (closed) {
                return;
            }
            logger.warn(`Companion resource ${proxyInfo.url} is not available: ${e}`);
            res.writeHead(e instanceof CompanionError && e.reason === 'timeout' ? 504 : 404);
            res.end();
        });
    }

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

                    if (isCompanionUrl(proxyInfo.url)) {
                        NetworkService.serveCompanion(req, res, proxyInfo);
                    }
                    else if (proxyInfo.url.startsWith('app://')) {
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

                        // Object.fromEntries is missing in the Node.js of webOS 5 and 6.
                        const filteredHeaders: { [key: string]: string } = {};
                        for (const key of Object.keys(req.headers)) {
                            if (!omitHeaders.has(key.toLowerCase())) {
                                const value = req.headers[key];
                                filteredHeaders[key] = Array.isArray(value) ? value.join(', ') : value;
                            }
                        }

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
        if (message.url && (message.url.startsWith('app://') || isCompanionUrl(message.url) || (message.headers && !streamingMediaTypes.find(v => v === message.container.toLocaleLowerCase())))) {
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
