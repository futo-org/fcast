import * as fs from 'fs';
import * as https from 'https';
import * as path from 'path';
import { URL } from 'url';

export class Updater {
    private basePath: string;
    private baseUrl: string;
    private appFiles: string[];

    constructor(basePath: string, baseUrl: string) {
        this.basePath = basePath;
        this.baseUrl = baseUrl;
        this.appFiles = [
            'dist/main/c.mp4',
            'dist/main/index.html',
            'dist/main/preload.js',
            'dist/main/qrcode.min.js',
            'dist/main/renderer.js',
            'dist/main/style.css',
            'dist/main/video-js.min.css',
            'dist/main/video.min.js',

            'dist/player/index.html',
            'dist/player/preload.js',
            'dist/player/renderer.js',
            'dist/player/style.css',
            'dist/player/video-js.min.css',
            'dist/player/video.min.js',

            'dist/app.ico',
            'dist/app.png',
            'dist/bundle.js',
            'package.json'
        ];
    }

    private async fetchJSON(url: string): Promise<any> {
        return new Promise((resolve, reject) => {
            https.get(url, (res) => {
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

    private async downloadFile(url: string, destination: string): Promise<void> {
        return new Promise((resolve, reject) => {
            const file = fs.createWriteStream(destination);
            https.get(url, (response) => {
                response.pipe(file);
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

    private compareVersions(v1: string, v2: string): number {
        const v1Parts = v1.split('.').map(Number);
        const v2Parts = v2.split('.').map(Number);

        for (let i = 0; i < v1Parts.length; i++) {
            if (v1Parts[i] > v2Parts[i]) {
                return 1;
            } else if (v1Parts[i] < v2Parts[i]) {
                return -1;
            }
        }

        return 0;
    }

    public async update(): Promise<Boolean> {
        console.log("Updater invoked", { baseUrl: this.baseUrl, basePath: this.basePath });

        const localPackage = JSON.parse(fs.readFileSync(path.join(this.basePath, './package.json'), 'utf-8'));
        const remotePackage = await this.fetchJSON(`${this.baseUrl}/package.json`.toString());

        console.log('Update check', { localVersion: localPackage.version, remoteVersion: remotePackage.version });
        if (this.compareVersions(remotePackage.version, localPackage.version) === 1) {
            for (const file of this.appFiles) {
                const fileUrl = `${this.baseUrl}/${file}`;
                const destination = path.join(this.basePath, file);

                console.log(`Downloading '${fileUrl}' to '${destination}'.`);
                await this.downloadFile(fileUrl.toString(), destination);
            }

            return true;
        }

        return false;
    }
}