/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { Opcode } from 'common/Packets';

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
        sendSessionMessage: (opcode: Opcode, message: any) => electronAPI.ipcRenderer.send('send-session-message', { opcode: opcode, message: message }),
        disconnectDevice: (session: string) => electronAPI.ipcRenderer.send('disconnect-device', session),
        onConnect: (callback: any) => electronAPI.ipcRenderer.on('connect', callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on('disconnect', callback),
        onPing: (callback: any) => electronAPI.ipcRenderer.on('ping', callback),
    });

// @ts-ignore
} else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
    preloadData = {
        onDeviceInfoCb: () => { console.log('Main: Callback not set while fetching device info'); },
        onConnectCb: (_, value: any) => { console.log('Main: Callback not set while calling onConnect'); },
        onDisconnectCb: (_, value: any) => { console.log('Main: Callback not set while calling onDisconnect'); },
        onPingCb: (_, value: any) => { console.log('Main: Callback not set while calling onPing'); },
    };

    window.targetAPI = {
        onDeviceInfo: (callback: () => void) => preloadData.onDeviceInfoCb = callback,
        onConnect: (callback: (_, value: any) => void) => preloadData.onConnectCb = callback,
        onDisconnect: (callback: (_, value: any) => void) => preloadData.onDisconnectCb = callback,
        onPing: (callback: (_, value: any) => void) => preloadData.onPingCb = callback,
        getDeviceInfo: () => preloadData.deviceInfo,
    };
} else {
    // @ts-ignore
    console.log(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
