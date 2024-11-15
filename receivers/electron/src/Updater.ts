import * as fs from 'fs';
import * as https from 'https';
import * as path from 'path';
import * as crypto from 'crypto';
import * as log4js from "log4js";
import { app } from 'electron';
import { Store } from './Store';
import sudo from 'sudo-prompt';
const extract = require('extract-zip');
const logger = log4js.getLogger();

enum UpdateState {
    Copy = 'copy',
    Cleanup = 'cleanup',
    Error = 'error',
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
    fileVersion: string;
}

interface UpdateInfo {
    updateState: UpdateState;
    installPath: string;
    tempPath: string;
    currentVersion: string;
    downloadFile: string;
    error?: string
}

export class Updater {
    private static readonly supportedReleasesJsonVersion = '1';

    private static appPath: string = app.getAppPath();
    private static installPath: string = path.join(Updater.appPath, '../../');
    private static updateDataPath: string = path.join(app.getPath('userData'), 'updater');
    private static updateMetadataPath = path.join(Updater.updateDataPath, './update.json');
    private static baseUrl: string = 'https://dl.fcast.org/electron';
    private static channelVersion: string = null;

    public static isDownloading: boolean = false;
    public static updateApplied: boolean = false;

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

    private static async applyUpdate(src: string, dst: string) {
        // Sanity removal protection check (especially under admin)
        if (!dst.includes('fcast-receiver')) {
            throw `Aborting update applying due to possible malformed path: ${dst}`;
        }

        try {
            fs.accessSync(dst, fs.constants.F_OK | fs.constants.R_OK | fs.constants.W_OK | fs.constants.X_OK);

            // Electron runtime sees .asar file as directory and causes errors during copy/remove operations
            process.noAsar = true
            fs.rmSync(dst, { recursive: true, force: true });
            fs.cpSync(src, dst, { recursive: true, force: true });
            process.noAsar = false
        }
        catch (err) {
            if (err.code === 'EACCES') {
                logger.info('Update requires admin privileges. Escalating...');

                await new Promise<void>((resolve, reject) => {
                    const shell = process.platform === 'win32' ? 'powershell' : '';
                    const command = `${shell} rm -rf ${dst}; ${shell} cp -rf ${src} ${dst}`

                    sudo.exec(command, { name: 'FCast Receiver' }, (error, stdout, stderr) => {
                        if (error)  {
                            logger.error(error);
                            logger.warn(`stdout: ${stdout}`);
                            logger.warn(`stderr: ${stderr}`);
                            reject('User did not authorize the operation...');
                        }

                        logger.info('stdout', stdout);
                        logger.info('stderr', stderr);
                        resolve();
                    });
                });
            }
            else {
                logger.error(err);
                throw err;
            }
        }
    }

    public static restart() {
        const updateInfo: UpdateInfo = JSON.parse(fs.readFileSync(Updater.updateMetadataPath, 'utf8'));
        const extractionDir = process.platform === 'darwin' ? 'FCast Receiver.app' : `fcast-receiver-${process.platform}-${process.arch}`;
        const binaryName = process.platform === 'win32' ? 'fcast-receiver.exe' : 'fcast-receiver';
        const updateBinPath = process.platform === 'darwin' ? path.join(updateInfo.tempPath, extractionDir) : path.join(updateInfo.tempPath, extractionDir, binaryName);

        app.relaunch({ execPath: updateBinPath });
        app.exit();
    }

    public static isUpdating(): boolean {
        try {
            const updateInfo: UpdateInfo = JSON.parse(fs.readFileSync(Updater.updateMetadataPath, 'utf8'));
            Updater.updateApplied = updateInfo.updateState === 'cleanup' ? true : false;
            return true;
        }
        catch {
            return false;
        }
    }

    public static getChannelVersion(): string {
        if (Updater.channelVersion === null) {
            const localPackage = JSON.parse(fs.readFileSync(path.join(Updater.appPath, './package.json'), 'utf8'));
            Updater.channelVersion = localPackage.channelVersion ? localPackage.channelVersion : 0
        }

        return Updater.channelVersion;
    }

    public static async processUpdate(): Promise<void> {
        try {
            const updateInfo: UpdateInfo = JSON.parse(fs.readFileSync(Updater.updateMetadataPath, 'utf8'));
            const extractionDir = process.platform === 'darwin' ? 'FCast Receiver.app' : `fcast-receiver-${process.platform}-${process.arch}`;

            switch (updateInfo.updateState) {
                case UpdateState.Copy: {
                    const binaryName = process.platform === 'win32' ? 'fcast-receiver.exe' : 'fcast-receiver';

                    try {
                        logger.info('Updater process started...');
                        const src = path.join(updateInfo.tempPath, extractionDir);
                        logger.info(`Copying files from update directory ${src} to install directory ${updateInfo.installPath}`);

                        Updater.applyUpdate(src, updateInfo.installPath);
                        updateInfo.updateState = UpdateState.Cleanup;
                        fs.writeFileSync(Updater.updateMetadataPath, JSON.stringify(updateInfo));

                        const installBinPath = path.join(updateInfo.installPath, binaryName);
                        log4js.shutdown();
                        app.relaunch({ execPath: installBinPath });
                        app.exit();
                    }
                    catch (err) {
                        logger.error('Error while applying update...');
                        logger.error(err);

                        updateInfo.updateState = UpdateState.Error;
                        updateInfo.error = JSON.stringify(err);
                        fs.writeFileSync(Updater.updateMetadataPath, JSON.stringify(updateInfo));
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
                        logger.info(`rm dir ${path.join(Updater.updateDataPath, extractionDir)}`)
                        fs.rmSync(path.join(Updater.updateDataPath, extractionDir), { recursive: true, force: true });
                        process.noAsar = false

                        fs.rmSync(path.join(Updater.updateDataPath, updateInfo.downloadFile));
                        fs.rmSync(Updater.updateMetadataPath);

                        // Removing the install directory causes an 'ENOENT: no such file or directory, uv_cwd' when calling process.cwd()
                        // Need to fix the working directory to the update directory that overwritten the install directory
                        process.chdir(Updater.installPath);
                    }
                    catch (err) {
                        logger.error('Error while performing update cleanup...');
                        logger.error(err);

                        updateInfo.updateState = UpdateState.Error;
                        updateInfo.error = JSON.stringify(err);
                        fs.writeFileSync(Updater.updateMetadataPath, JSON.stringify(updateInfo));
                    }

                    return;
                }

                case UpdateState.Error:
                    logger.warn(`Update operation did not complete successfully: ${updateInfo.error}`);
                    break;
            }
        }
        catch (err) {
            logger.warn(`Error reading update metadata file, ignoring pending update: ${err}`);
        }
    }

    public static async update(): Promise<boolean> {
        logger.info('Updater invoked');
        try {
            fs.accessSync(Updater.updateDataPath, fs.constants.F_OK);
        }
        catch (err) {
            logger.info(`Directory does not exist: ${err}`);
            fs.mkdirSync(Updater.updateDataPath);
        }

        const localPackage = JSON.parse(fs.readFileSync(path.join(Updater.appPath, './package.json'), 'utf8'));
        try {
            const releases = await Updater.fetchJSON(`${Updater.baseUrl}/releases_v${Updater.supportedReleasesJsonVersion}.json`.toString()) as ReleaseInfo;

            let updaterSettings = Store.get('updater');
            if (updaterSettings === null) {
                updaterSettings = {
                    'channel': localPackage.channel,
                }

                Store.set('updater', updaterSettings);
            }

            const localChannelVersion: number = localPackage.channelVersion ? localPackage.channelVersion : 0
            const currentChannelVersion: number = releases.channelCurrentVersions[localPackage.channel] ? releases.channelCurrentVersions[localPackage.channel] : 0
            logger.info('Update check', { channel: localPackage.channel, channel_version: localChannelVersion, localVersion: localPackage.version,
                currentVersion: releases.currentVersion, currentChannelVersion: currentChannelVersion });

            if (localPackage.version !== releases.currentVersion || (localPackage.channel !== 'stable' && localChannelVersion < currentChannelVersion)) {
                const channel = localPackage.version !== releases.currentVersion ? 'stable' : localPackage.channel;
                const fileInfo = releases.currentReleases[channel][process.platform][process.arch]
                const file = fileInfo.url.toString().split('/').pop();

                const destination = path.join(Updater.updateDataPath, file);
                logger.info(`Downloading '${fileInfo.url}' to '${destination}'.`);
                Updater.isDownloading = true;
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
                process.noAsar = true;
                await extract(destination, { dir: path.dirname(destination) });
                process.noAsar = false;

                logger.info('Extraction complete.');
                const updateInfo: UpdateInfo = {
                    updateState: UpdateState.Copy,
                    installPath: Updater.installPath,
                    tempPath: path.dirname(destination),
                    currentVersion: releases.currentVersion,
                    downloadFile: file,
                };

                fs.writeFileSync(Updater.updateMetadataPath, JSON.stringify(updateInfo));
                logger.info('Written update metadata.');
                Updater.isDownloading = false;
                return true;
            }
        }
        catch (err) {
            Updater.isDownloading = false;
            process.noAsar = false;
            logger.error(`Failed to check for updates: ${err}`);
            throw 'Failed to check for updates. Please try again later or visit https://fcast.org for updates.';
        }

        return false;
    }
}
