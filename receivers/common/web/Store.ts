/* eslint-disable @typescript-eslint/no-explicit-any */
import * as fs from 'fs';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('Store', LoggerType.BACKEND);

export interface NetworkSettings {
    ignoreCertificateErrors: boolean,
    deviceName: string,
}

export interface UISettings {
    mainWindowBackground: string,
}

export interface UpdaterSettings {
    channel: string,
    checkForUpdatesOnStart: boolean,
}

export interface Settings {
    storeVersion: number,
    network: NetworkSettings,
    ui: UISettings,
    updater: UpdaterSettings,
}

export class Store {
    private static instance: Store = null;
    private static storeVersion = 1;
    private static path?: string = null;
    public static settings: Settings = null;

    constructor(path?: string) {
        if (!Store.instance) {
            // @ts-ignore
            if (TARGET === 'electron') {
                Store.path = path;
                Store.settings = JSON.parse(fs.readFileSync(path, { encoding: 'utf8', flag: 'r' })) as Settings;
                logger.info('Read settings file:', Store.settings);

                if (Store.settings === undefined) {
                    Store.settings.storeVersion = Store.storeVersion;
                    fs.writeFileSync(Store.path, JSON.stringify(Store.settings));
                }

            // @ts-ignore
            } else if (TARGET === 'webOS') {
                // todo
            } else {
                // @ts-ignore
                logger.warn(`Attempting to initialize Store on unsupported target: ${TARGET}`);
            }

            Store.instance = this;
        }
    }

    public static saveSettings() {
        // @ts-ignore
        if (TARGET === 'electron') {
            logger.info('Saving settings file:', Store.settings);
            fs.writeFileSync(Store.path, JSON.stringify(Store.settings));

        // @ts-ignore
        } else if (TARGET === 'webOS') {
            // todo
        } else {
            // @ts-ignore
            logger.warn(`Attempting to initialize Store on unsupported target: ${TARGET}`);
        }
    }
}
