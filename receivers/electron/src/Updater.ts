import * as fs from 'fs';
import * as https from 'https';
import * as path from 'path';
import * as crypto from 'crypto';
import * as log4js from "log4js";
import { app } from 'electron';
import { Store } from './Store';
const extract = require('extract-zip');
const logger = log4js.getLogger();

enum UpdateState {
    Copy,
    Cleanup,
};

interface ReleaseInfo {
    previousVersions: [string];
    currentVersion: string;
    currentReleases: [
        string: [ // channel
            string: [ // os
                string: [ // arch
                    string: []
                ]
            ]
        ]
    ];
    channelCurrentVersions: [string: number];
    allVersions: [string];
}

interface UpdateInfo {
    updateState: UpdateState;
    installPath: string;
    tempPath: string;
    currentVersion: string;
}

export class Updater {
    private static appPath: string = app.getAppPath();
    private static installPath: string = path.join(Updater.appPath, '../../');
    private static updateDataPath: string = path.join(app.getPath('userData'), 'updater');
    private static updateMetadataPath = path.join(Updater.updateDataPath, './update.json');
    private static baseUrl: string = 'https://dl.fcast.org/electron';

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    private static async fetchJSON(url: string): Promise<any> {
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

    private static async downloadFile(url: string, destination: string): Promise<void> {
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

    private static getDownloadFile(version: string) {
        let target: string = process.platform; // linux

        if (process.platform === 'win32') {
            target = 'windows';
        }
        else if (process.platform === 'darwin') {
            target = 'macOS';
        }

        return `fcast-receiver-${version}-${target}-${process.arch}.zip`;
    }

    public static isUpdating() {
        return fs.existsSync(Updater.updateMetadataPath);
    }

    public static async processUpdate(): Promise<void> {
        const updateInfo: UpdateInfo = JSON.parse(fs.readFileSync(Updater.updateMetadataPath, 'utf-8'));
        const extractionDir = process.platform === 'darwin' ? 'FCast Receiver.app' : `fcast-receiver-${process.platform}-${process.arch}`;

        switch (updateInfo.updateState) {
            case UpdateState.Copy: {
                const binaryName = process.platform === 'win32' ? 'fcast-receiver.exe' : 'fcast-receiver';

                if (Updater.installPath === updateInfo.installPath) {
                    logger.info('Update in progress. Restarting application to perform update...')
                    const updateBinPath = process.platform === 'darwin' ? path.join(updateInfo.tempPath, extractionDir) : path.join(updateInfo.tempPath, extractionDir, binaryName);

                    log4js.shutdown();
                    app.relaunch({ execPath: updateBinPath });
                    app.exit();
                }

                try {
                    logger.info('Updater process started...');
                    const src = path.join(updateInfo.tempPath, extractionDir);
                    logger.info(`Copying files from update directory ${src} to install directory ${updateInfo.installPath}`);

                    // Electron runtime sees .asar file as directory and causes errors during copy
                    process.noAsar = true
                    fs.cpSync(src, updateInfo.installPath, { recursive: true, force: true });
                    process.noAsar = false

                    updateInfo.updateState = UpdateState.Cleanup;
                    await fs.promises.writeFile(Updater.updateMetadataPath, JSON.stringify(updateInfo));

                    const installBinPath = path.join(updateInfo.installPath, binaryName);
                    log4js.shutdown();
                    app.relaunch({ execPath: installBinPath });
                    app.exit();
                }
                catch (err) {
                    logger.error('Error while applying update...');
                    logger.error(err);
                    log4js.shutdown();
                    app.exit();
                }

                return;
            }

            case UpdateState.Cleanup: {
                try {
                    logger.info('Performing update cleanup...')
                    // Electron runtime sees .asar file as directory and causes errors during copy
                    process.noAsar = true
                    fs.rmSync(path.join(Updater.updateDataPath, extractionDir), { recursive: true, force: true });
                    process.noAsar = false

                    fs.rmSync(path.join(Updater.updateDataPath, Updater.getDownloadFile(updateInfo.currentVersion)));
                    fs.rmSync(Updater.updateMetadataPath);
                }
                catch (err) {
                    logger.error('Error while performing update cleanup...');
                    logger.error(err);
                }

                log4js.shutdown();
                app.relaunch();
                app.exit();

                return;
            }
        }
    }

    public static async update(): Promise<boolean> {
        logger.info('Updater invoked');
        if (!fs.existsSync(Updater.updateDataPath)) {
            fs.mkdirSync(Updater.updateDataPath);
        }

        const localPackage = JSON.parse(fs.readFileSync(path.join(Updater.appPath, './package.json'), 'utf-8'));
        const releases = await Updater.fetchJSON(`${Updater.baseUrl}/releases.json`.toString()) as ReleaseInfo;

        let updaterSettings = Store.get('updater');
        if (updaterSettings === null) {
            updaterSettings = {
                'channel': localPackage.channel,
            }

            Store.set('updater', updaterSettings);
        }

        const localChannelVersion: number = localPackage.channelVersion ? localPackage.channelVersion : 0
        const currentChannelVersion: number = releases.channelCurrentVersions[localPackage.channel] ? releases.channelCurrentVersions[localPackage.channel] : 0
        logger.info('Update check', { channel: localPackage.channel, localVersion: localPackage.version, currentVersion: releases.currentVersion });

        if (localPackage.version !== releases.currentVersion || (localPackage.channel !== 'stable' && localChannelVersion < currentChannelVersion)) {
            const channel = localPackage.version !== releases.currentVersion ? 'stable' : localPackage.channel;
            const file = Updater.getDownloadFile(releases.currentVersion);
            const fileInfo = releases.currentReleases[channel][process.platform][process.arch]

            const destination = path.join(Updater.updateDataPath, file);
            logger.info(`Downloading '${fileInfo.url}' to '${destination}'.`);
            await Updater.downloadFile(fileInfo.url.toString(), destination);

            const downloadedFile = await fs.promises.readFile(destination);
            const hash = crypto.createHash('sha256').end(downloadedFile).digest('hex');
            if (fileInfo.sha256Digest !== hash) {
                const message = 'Update failed integrity check. Please try checking for updates again or downloading the update manually.';
                logger.error(`Update failed integrity check. Expected hash: ${fileInfo.sha256Digest}, actual hash: ${hash}`);
                throw message;
            }

            // Electron runtime sees .asar file as directory and causes errors during extraction
            logger.info('Extracting update...');
            process.noAsar = true
            await extract(destination, { dir: path.dirname(destination) });
            process.noAsar = false

            logger.info('Extraction complete.');
            const updateInfo: UpdateInfo = {
                updateState: UpdateState.Copy,
                installPath: Updater.installPath,
                tempPath: path.dirname(destination),
                currentVersion: releases.currentVersion,
            };

            await fs.promises.writeFile(Updater.updateMetadataPath, JSON.stringify(updateInfo));
            logger.info('Written update metadata.');
            return true;
        }

        return false;
    }
}
