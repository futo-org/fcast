/* eslint-disable @typescript-eslint/no-explicit-any */
import storage from 'electron-json-storage';
import { app } from 'electron';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('Store', LoggerType.BACKEND);

export class Store {
    private static storeVersion = 1;
    private static userSettings = 'UserSettings';
    private static settingsCache: any = null;

    static {
        storage.setDataPath(app.getPath('userData'));
        Store.settingsCache = storage.getSync(Store.userSettings);

        if (Store.get('storeVersion') === null) {
            Store.set('storeVersion', Store.storeVersion);
        }
    }

    public static get(key: string): any {
        return Store.settingsCache[key] ?? null;
    }

    public static set(key: string, value: any) {
        Store.settingsCache[key] = value;

        logger.info(`Writing settings file: key '${key}', value ${JSON.stringify(value)}`);
        storage.set(Store.userSettings,  Store.settingsCache, (err) => {
            if (err) {
                logger.error(`Error writing user settings: ${err}`);
            }
        });
    }
}
