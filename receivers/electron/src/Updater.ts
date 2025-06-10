import * as fs from 'fs';
import * as path from 'path';
import * as crypto from 'crypto';
import { app } from 'electron';
import { Store } from './Store';
import sudo from 'sudo-prompt';
import { Logger, LoggerType } from 'common/Logger';
import { fetchJSON, downloadFile } from 'common/UtilityBackend';

const cp = require('child_process');
const extract = require('extract-zip');
const logger = new Logger('Updater', LoggerType.BACKEND);

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

interface UpdateConditions {
    newVersion: boolean,
    newChannelVersion: boolean,
    newCommit: boolean,
}

export class Updater {
    private static readonly supportedReleasesJsonVersion = '1';

    private static appPath: string = app.getAppPath();
    private static installPath: string = process.platform === 'darwin' ? path.join(Updater.appPath, '../../../') : path.join(Updater.appPath, '../../');
    private static updateDataPath: string = path.join(app.getPath('userData'), 'updater');
    private static updateMetadataPath = path.join(Updater.updateDataPath, './update.json');
    private static baseUrl: string = 'https://dl.fcast.org/electron';
    private static isRestarting: boolean = false;
    private static updateConditions: UpdateConditions;

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    private static localPackageJson: any = null;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    private static releasesJson: any = null;

    public static isDownloading: boolean = false;
    public static updateError: boolean = false;
    public static updateDownloaded: boolean = false;
    public static updateProgress: number = 0;
    public static checkForUpdatesOnStart: boolean = true;
    public static releaseChannel = 'stable';
    public static updateChannel = 'stable';

    static {
        Updater.localPackageJson = JSON.parse(fs.readFileSync(path.join(Updater.appPath, './package.json'), 'utf8'));

        let updaterSettings = Store.get('updater');
        if (updaterSettings !== null) {
            Updater.updateChannel = updaterSettings.channel === undefined ? Updater.localPackageJson.channel : updaterSettings.channel;
            Updater.checkForUpdatesOnStart = updaterSettings.checkForUpdatesOnStart === undefined ? true : updaterSettings.checkForUpdatesOnStart;
        }

        updaterSettings = {
            'channel': Updater.updateChannel,
            'checkForUpdatesOnStart': Updater.checkForUpdatesOnStart,
        }

        Updater.releaseChannel = Updater.localPackageJson.channel;
        Store.set('updater', updaterSettings);
    }

    private static async applyUpdate(src: string, dst: string) {
        try {
            fs.accessSync(dst, fs.constants.F_OK | fs.constants.R_OK | fs.constants.W_OK | fs.constants.X_OK);

            // Electron runtime sees .asar file as directory and causes errors during copy/remove operations
            process.noAsar = true
            if (process.platform === 'win32') {
                // Cannot remove top-level directory since it might still be locked...
                fs.rmSync(`${dst}\\*`, { maxRetries: 5, retryDelay: 1000, recursive: true, force: true });
            }
            else {
                fs.rmSync(dst, { maxRetries: 5, retryDelay: 1000, recursive: true, force: true });
            }

            if (process.platform === 'darwin') {
                // Electron framework libraries break otherwise on Mac
                fs.cpSync(src, dst, { recursive: true, force: true, verbatimSymlinks: true });
            }
            else {
                fs.cpSync(src, dst, { recursive: true, force: true });
            }
        }
        catch (err) {
            if (err.code === 'EACCES' || err.code === 'EPERM') {
                logger.info('Update requires admin privileges. Escalating...');

                await new Promise<void>((resolve, reject) => {
                    let command: string;
                    if (process.platform === 'win32') {
                        // Using native cmd.exe seems to create less issues than using powershell...
                        command = `rmdir /S /Q "${dst}" & xcopy /Y /E "${src}" "${dst}"`;
                    }
                    else {
                        command = `rm -rf '${dst}'; cp -rf '${src}' '${dst}'; chmod 755 '${dst}'`;
                    }

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
        finally {
            process.noAsar = false;
        }
    }

    // Cannot use app.relaunch(...) since it breaks privilege escalation on Linux...
    // Also does not work very well on Mac...
    private static relaunch(binPath: string) {
        logger.info(`Relaunching app binary: ${binPath}`);
        logger.shutdown();

        let proc;
        if (process.platform === 'win32') {
            // cwd is bugged on Windows, perhaps due to needing to be in system32 to launch cmd.exe
            proc = cp.spawn(`"${binPath}"`, [], { stdio: 'ignore', shell: true, detached: true, windowsHide: true });
        }
        else if (process.platform === 'darwin') {
            proc = cp.spawn(`open '${binPath}'`, [], { cwd: path.dirname(binPath), shell: true, stdio: 'ignore', detached: true });
        }
        else {
            proc = cp.spawn(binPath, [], { cwd: path.dirname(binPath), shell: true, stdio: 'ignore', detached: true });
        }

        proc.unref();
        app.exit();
        return;
    }

    private static compareVersions(v1: string, v2: string): number {
        const v1Parts = v1.split('.').map(Number);
        const v2Parts = v2.split('.').map(Number);

        for (let i = 0; i < v1Parts.length; i++) {
            if (v1Parts[i] > v2Parts[i]) {
                return 1;
            }
            else if (v1Parts[i] < v2Parts[i]) {
                return -1;
            }
        }

        return 0;
    }

    public static restart() {
        if (!Updater.isRestarting) {
            Updater.isRestarting = true;
            const updateInfo: UpdateInfo = JSON.parse(fs.readFileSync(Updater.updateMetadataPath, 'utf8'));
            const extractionDir = process.platform === 'darwin' ? 'FCast Receiver.app' : `fcast-receiver-${process.platform}-${process.arch}`;
            const binaryName = process.platform === 'win32' ? 'fcast-receiver.exe' : 'fcast-receiver';
            const updateBinPath = process.platform === 'darwin' ? path.join(updateInfo.tempPath, extractionDir) : path.join(updateInfo.tempPath, extractionDir, binaryName);

            Updater.relaunch(updateBinPath);
        }

        return;
    }

    public static isUpdating(): boolean {
        try {
            const updateInfo: UpdateInfo = JSON.parse(fs.readFileSync(Updater.updateMetadataPath, 'utf8'));
            Updater.updateError = true;
            return updateInfo.updateState !== 'error';
        }
        catch {
            return false;
        }
    }

    public static getChannelVersion(): string {
        Updater.localPackageJson.channelVersion = Updater.localPackageJson.channelVersion ? Updater.localPackageJson.channelVersion : 0
        return Updater.localPackageJson.channelVersion;
    }

    public static getCommit(): string {
        Updater.localPackageJson.commit = Updater.localPackageJson.commit ? Updater.localPackageJson.commit : null
        return Updater.localPackageJson.commit;
    }

    public static async processUpdate(): Promise<void> {
        try {
            const updateInfo: UpdateInfo = JSON.parse(fs.readFileSync(Updater.updateMetadataPath, 'utf8'));
            const extractionDir = process.platform === 'darwin' ? 'FCast Receiver.app' : `fcast-receiver-${process.platform}-${process.arch}`;
            const binaryName = process.platform === 'win32' ? 'fcast-receiver.exe' : 'fcast-receiver';
            const installBinPath = process.platform === 'darwin' ? updateInfo.installPath : path.join(updateInfo.installPath, binaryName);

            switch (updateInfo.updateState) {
                case UpdateState.Copy: {
                    try {
                        logger.info('Updater process started...');
                        const src = path.join(updateInfo.tempPath, extractionDir);
                        logger.info(`Copying files from update directory ${src} to install directory ${updateInfo.installPath}`);

                        await Updater.applyUpdate(src, updateInfo.installPath);
                        updateInfo.updateState = UpdateState.Cleanup;
                        fs.writeFileSync(Updater.updateMetadataPath, JSON.stringify(updateInfo));
                    }
                    catch (err) {
                        logger.error('Error while applying update...');
                        logger.error(err);

                        updateInfo.updateState = UpdateState.Error;
                        updateInfo.error = JSON.stringify(err);
                        fs.writeFileSync(Updater.updateMetadataPath, JSON.stringify(updateInfo));
                    }

                    Updater.relaunch(installBinPath);
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

                    Updater.relaunch(installBinPath);
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

    public static async checkForUpdates(): Promise<boolean> {
        logger.info('Checking for updates...');

        try {
            Updater.releasesJson = await fetchJSON(`${Updater.baseUrl}/releases_v${Updater.supportedReleasesJsonVersion}.json`.toString()) as ReleaseInfo;

            const localChannelVersion: number = Updater.localPackageJson.channelVersion ? Updater.localPackageJson.channelVersion : 0;
            const currentChannelVersion: number = Updater.releasesJson.channelCurrentVersions[Updater.updateChannel] ? Updater.releasesJson.channelCurrentVersions[Updater.updateChannel] : 0;
            logger.info('Update check', {
                updateChannel: Updater.updateChannel,
                channel_version: localChannelVersion,
                localVersion: Updater.localPackageJson.version,
                currentVersion: Updater.releasesJson.currentVersion,
                currentCommit: Updater.releasesJson.currentCommit,
                currentChannelVersion: currentChannelVersion
            });

            const newVersion = Updater.compareVersions(Updater.localPackageJson.version, Updater.releasesJson.currentVersion) < 0;

            // Note: Major version updates are not captured in this check (e.g. 1.0.0-beta-4 -> 2.0.0-beta-1 being a valid update yet rejected)
            const newChannelVersion = (Updater.updateChannel !== 'stable' && localChannelVersion < currentChannelVersion);

            // Allow for update promotion to stable, while still getting updates from the subscribed channel
            const newCommit = (
                Updater.updateChannel !== 'stable' &&
                Updater.localPackageJson.commit !== Updater.releasesJson.currentCommit &&
                Updater.localPackageJson.version === Updater.releasesJson.currentVersion &&
                localChannelVersion === currentChannelVersion
            );

            Updater.updateConditions = {
                newVersion: newVersion,
                newChannelVersion: newChannelVersion,
                newCommit: newCommit,
            };

            // Prevent downgrading to sub channel if on stable
            const isUpdateToStable = newVersion || newCommit;
            const isDowngrade = Updater.releaseChannel === 'stable' && !isUpdateToStable && newChannelVersion;

            if ((newVersion || newChannelVersion || newCommit) && !isDowngrade) {
                logger.info('Update available...', Updater.updateConditions);
                return true;
            }
        }
        catch (err) {
            logger.error(`Failed to check for updates: ${err}`);
            throw 'Please try again later or visit https://fcast.org for updates.';
        }

        return false;
    }

    public static async downloadUpdate(): Promise<boolean> {
        try {
            fs.accessSync(Updater.updateDataPath, fs.constants.F_OK);
        }
        catch (err) {
            logger.info(`Directory does not exist: ${err}`);
            fs.mkdirSync(Updater.updateDataPath);
        }

        try {
            const channel = (Updater.updateConditions.newVersion || Updater.updateConditions.newCommit) ? 'stable' : Updater.updateChannel;
            const fileInfo = Updater.releasesJson.currentReleases[channel][process.platform][process.arch]
            const file = fileInfo.url.toString().split('/').pop();

            const destination = path.join(Updater.updateDataPath, file);
            logger.info(`Downloading '${fileInfo.url}' to '${destination}'.`);
            Updater.isDownloading = true;
            await downloadFile(fileInfo.url.toString(), destination, null, (downloadedBytes: number, downloadSize: number) => {
                Updater.updateProgress = downloadedBytes / downloadSize;
            });

            const downloadedFile = await fs.promises.readFile(destination);
            const hash = crypto.createHash('sha256').end(downloadedFile).digest('hex');
            if (fileInfo.sha256Digest !== hash) {
                const message = 'Update failed integrity check. Please try again later or visit https://fcast.org to for updates.';
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
                currentVersion: Updater.releasesJson.currentVersion,
                downloadFile: file,
            };

            fs.writeFileSync(Updater.updateMetadataPath, JSON.stringify(updateInfo));
            logger.info('Written update metadata.');
            Updater.isDownloading = false;
            Updater.updateDownloaded = true;
            return true;
        }
        catch (err) {
            Updater.isDownloading = false;
            process.noAsar = false;
            logger.error(`Failed to download update: ${err}`);
            throw 'Failed to download update. Please try again later or visit https://fcast.org to download.';
        }
    }
}
