/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { Opcode } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
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

// @ts-ignore
if (TARGET === 'electron') {
    // @ts-ignore
    const electronAPI = __non_webpack_require__('electron');

    electronAPI.ipcRenderer.on("device-info", (_event, value: any) => {
        preloadData.deviceInfo = value;
    })

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        onDeviceInfo: (callback: any) => electronAPI.ipcRenderer.on('device-info', callback),
        getDeviceInfo: () => preloadData.deviceInfo,
        getSessions: () => electronAPI.ipcRenderer.invoke('get-sessions'),
        sendSessionMessage: (opcode: Opcode, message: any) => electronAPI.ipcRenderer.send('send-session-message', { opcode: opcode, message: message }),
        disconnectDevice: (session: string) => electronAPI.ipcRenderer.send('disconnect-device', session),
        onConnect: (callback: any) => electronAPI.ipcRenderer.on('connect', callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on('disconnect', callback),
        onPing: (callback: any) => electronAPI.ipcRenderer.on('ping', callback),
        onPong: (callback: any) => electronAPI.ipcRenderer.on('pong', callback),
        logger: loggerInterface,
    });

// @ts-ignore
} else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
    preloadData = {
        onDeviceInfoCb: () => { logger.error('Main: Callback not set while fetching device info'); },
        onConnectCb: (_, value: any) => { logger.error('Main: Callback not set while calling onConnect'); },
        onDisconnectCb: (_, value: any) => { logger.error('Main: Callback not set while calling onDisconnect'); },
        onPingCb: (_, value: any) => { logger.error('Main: Callback not set while calling onPing'); },
    };

    window.targetAPI = {
        onDeviceInfo: (callback: () => void) => preloadData.onDeviceInfoCb = callback,
        onConnect: (callback: (_, value: any) => void) => preloadData.onConnectCb = callback,
        onDisconnect: (callback: (_, value: any) => void) => preloadData.onDisconnectCb = callback,
        onPing: (callback: (_, value: any) => void) => preloadData.onPingCb = callback,
        getDeviceInfo: () => preloadData.deviceInfo,
        logger: loggerInterface,
    };
} else {
    // @ts-ignore
    logger.warn(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
