/* eslint-disable @typescript-eslint/no-explicit-any */
import * as fs from 'fs';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('Settings', LoggerType.BACKEND);

export interface LoggerSettings {
    level: string,
}

export interface NetworkSettings {
    ignoreCertificateErrors: boolean,
    deviceName: string,
}

export interface UISettings {
    noMainWindow: boolean,
    fullscreen: boolean,
    mainWindowBackground: string,
}

export interface UpdaterSettings {
    channel: string,
    checkForUpdatesOnStart: boolean,
}

export interface Settings {
    storeVersion: number,
    log: LoggerSettings,
    network: NetworkSettings,
    ui: UISettings,
    updater: UpdaterSettings,
}

export class Settings {
    private static instance: Settings = null;
    private static readonly version = 1;
    private static path?: string = null;
    public static json: Settings;


    constructor(path?: string) {
        if (!Settings.instance) {
            // @ts-ignore
            if (TARGET === 'electron') {
                Settings.path = path;
                Settings.json = JSON.parse(fs.readFileSync(path, { encoding: 'utf8', flag: 'r' })) as Settings;
                logger.info('Read settings file:', Settings.json);
                Settings.setDefault();

            // @ts-ignore
            } else if (TARGET === 'webOS') {
                // todo
            } else {
                // @ts-ignore
                logger.warn(`Attempting to initialize Settings on unsupported target: ${TARGET}`);
            }

            Settings.instance = this;
        }
    }

    private static setDefault() {
        Settings.json.storeVersion = Settings.json.storeVersion === undefined ? Settings.version : Settings.json.storeVersion;
        Settings.json.log = Settings.json.log === undefined ? {} as LoggerSettings : Settings.json.log;
        Settings.json.network = Settings.json.network === undefined ? {} as NetworkSettings : Settings.json.network;
        Settings.json.ui = Settings.json.ui === undefined ? {} as UISettings : Settings.json.ui;
        Settings.json.updater = Settings.json.updater === undefined ? {} as UpdaterSettings : Settings.json.updater;

        Settings.json.log.level = Settings.json.log.level === undefined ? 'INFO' : Settings.json.log.level;

        Settings.json.network.deviceName = Settings.json.network.deviceName === undefined ? '' : Settings.json.network.deviceName;
        Settings.json.network.ignoreCertificateErrors = Settings.json.network.ignoreCertificateErrors === undefined ? false : Settings.json.network.ignoreCertificateErrors;

        Settings.json.ui.noMainWindow = Settings.json.ui.noMainWindow === undefined ? false : Settings.json.ui.noMainWindow;
        Settings.json.ui.fullscreen = Settings.json.ui.fullscreen === undefined ? false : Settings.json.ui.fullscreen;
        Settings.json.ui.mainWindowBackground = Settings.json.ui.mainWindowBackground === undefined ? '' : Settings.json.ui.mainWindowBackground;

        Settings.json.updater.channel = Settings.json.updater.channel === undefined ? '' : Settings.json.updater.channel;
        Settings.json.updater.checkForUpdatesOnStart = Settings.json.updater.checkForUpdatesOnStart === undefined ? true : Settings.json.updater.checkForUpdatesOnStart;

        Settings.save();
    }

    public static save() {
        // @ts-ignore
        if (TARGET === 'electron') {
            logger.info('Saving settings file:', Settings.json);
            fs.writeFileSync(Settings.path, JSON.stringify(Settings.json, null, 4));

        // @ts-ignore
        } else if (TARGET === 'webOS') {
            // todo
        } else {
            // @ts-ignore
            logger.warn(`Attempting to initialize Settings on unsupported target: ${TARGET}`);
        }
    }
}
