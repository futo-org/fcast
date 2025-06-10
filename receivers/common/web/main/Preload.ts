/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { Logger, LoggerType } from 'common/Logger';
import { EventMessage } from 'common/Packets';
const logger = new Logger('MainWindow', LoggerType.FRONTEND);

// Cannot directly pass the object to the renderer for some reason...
const loggerInterface = {
    trace: (message?: any, ...optionalParams: any[]) => { logger.trace(message, ...optionalParams); },
    debug: (message?: any, ...optionalParams: any[]) => { logger.debug(message, ...optionalParams); },
    info: (message?: any, ...optionalParams: any[]) => { logger.info(message, ...optionalParams); },
    warn: (message?: any, ...optionalParams: any[]) => { logger.warn(message, ...optionalParams); },
    error: (message?: any, ...optionalParams: any[]) => { logger.error(message, ...optionalParams); },
    fatal: (message?: any, ...optionalParams: any[]) => { logger.fatal(message, ...optionalParams); },
};

declare global {
    interface Window {
      electronAPI: any;
      webOS: any;
      webOSDev: any;
      targetAPI: any;
    }
}

let preloadData: Record<string, any> = {};
preloadData.subscribedKeys = {
    keyDown: new Set<string>(),
    keyUp: new Set<string>(),
};

// @ts-ignore
if (TARGET === 'electron') {
    // @ts-ignore
    const electronAPI = __non_webpack_require__('electron');

    electronAPI.ipcRenderer.on("device-info", (_event, value: any) => {
        preloadData.deviceInfo = value;
    })
    electronAPI.ipcRenderer.on("event-subscribed-keys-update", (_event, value: { keyDown: Set<string>, keyUp: Set<string> }) => {
        preloadData.subscribedKeys.keyDown = value.keyDown;
        preloadData.subscribedKeys.keyUp = value.keyUp;
    })

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        onDeviceInfo: (callback: any) => electronAPI.ipcRenderer.on('device-info', callback),
        getDeviceInfo: () => preloadData.deviceInfo,
        getSessions: () => electronAPI.ipcRenderer.invoke('get-sessions'),
        getSubscribedKeys: () => preloadData.subscribedKeys,
        onConnect: (callback: any) => electronAPI.ipcRenderer.on('connect', callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on('disconnect', callback),
        sendEvent: (message: EventMessage) => electronAPI.ipcRenderer.send('send-event', message),
        logger: loggerInterface,
    });

// @ts-ignore
} else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
    preloadData = {
        onDeviceInfoCb: () => { logger.error('Main: Callback not set while fetching device info'); },
        getSessionsCb: () => { logger.error('Main: Callback not set while calling getSessions'); },
        onConnectCb: (_, value: any) => { logger.error('Main: Callback not set while calling onConnect'); },
        onDisconnectCb: (_, value: any) => { logger.error('Main: Callback not set while calling onDisconnect'); },
    };

    window.targetAPI = {
        onDeviceInfo: (callback: () => void) => preloadData.onDeviceInfoCb = callback,
        getDeviceInfo: () => preloadData.deviceInfo,
        getSessions: (callback?: () => Promise<[any]>) => {
            if (callback) {
                preloadData.getSessionsCb = callback;
            }
            else {
                return preloadData.getSessionsCb();
            }
        },
        onConnect: (callback: (_, value: any) => void) => preloadData.onConnectCb = callback,
        onDisconnect: (callback: (_, value: any) => void) => preloadData.onDisconnectCb = callback,
        logger: loggerInterface,
    };
} else {
    // @ts-ignore
    logger.warn(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
