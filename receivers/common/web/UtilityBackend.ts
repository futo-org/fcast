import * as fs from 'fs';
import * as url from 'url';
import { http, https } from 'modules/follow-redirects';
import * as memfs from 'modules/memfs';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('UtilityBackend', LoggerType.BACKEND);

export function deepEqual(x, y) {
    const ok = Object.keys, tx = typeof x, ty = typeof y;
    return x && y && tx === 'object' && tx === ty ? (
        ok(x).length === ok(y).length &&
        ok(x).every(key => deepEqual(x[key], y[key]))
    ) : (x === y);
}

export async function fetchJSON(url: string): Promise<any> {
    const protocol = url.startsWith('https') ? https : http;

    return new Promise((resolve, reject) => {
        protocol.get(url, (res) => {
            let data = '';
            res.on('data', (chunk) => {
                data += chunk;
            });

            res.on('end', () => {
                try {
                    resolve(JSON.parse(data));
                } catch (err) {
                    reject(err);
                }
            });
        }).on('error', (err) => {
            reject(err);
        });
    });
}

export async function downloadFile(downloadUrl: string, destination: string, inMemory: boolean = false, requestHeaders: { [key: string]: string } = null,
                                   startCb: (downloadSize: number) => boolean = null,
                                   progressCb: (downloadedBytes: number, downloadSize: number) => void = null): Promise<void> {
    return new Promise((resolve, reject) => {
        const file = inMemory ? memfs.fs.createWriteStream(destination) : fs.createWriteStream(destination);
        const protocol = downloadUrl.startsWith('https') ? https : http;

        const parsedUrl = url.parse(downloadUrl);
        const options = protocol.RequestOptions = {
            ...parsedUrl,
            headers: requestHeaders
        };

        protocol.get(options, (response) => {
            const downloadSize = Number(response.headers['content-length']);
            logger.info(`Downloading file ${downloadUrl} to ${destination} with size: ${downloadSize} bytes`);
            if (startCb) {
                if (!startCb(downloadSize)) {
                    file.close();
                    reject('Error: Aborted download');
                }
            }

            response.pipe(file);
            let downloadedBytes = 0;

            response.on('data', (chunk) => {
                downloadedBytes += chunk.length;
                if (progressCb) {
                    progressCb(downloadedBytes, downloadSize);
                }
            });
            file.on('finish', () => {
                file.close();
                resolve();
            });
        }).on('error', (err) => {
            file.close();
            reject(err);
        });
    });
}
