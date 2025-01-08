/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { toast, ToastIcon } from '../components/Toast';

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

    // Since event is sent async during window startup, could fire off before or after renderer.js is loaded
    electronAPI.ipcRenderer.on('startup-storage-clear', () => {
        localStorage.clear();
    });

    electronAPI.ipcRenderer.on("device-info", (_event, value: any) => {
        preloadData.deviceInfo = value;
    })

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        onStartupStorageClear: (callback: any) => electronAPI.ipcRenderer.on('startup-storage-clear', callback),
        onDeviceInfo: (callback: any) => electronAPI.ipcRenderer.on("device-info", callback),
        onConnect: (callback: any) => electronAPI.ipcRenderer.on("connect", callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on("disconnect", callback),
        getDeviceInfo: () => preloadData.deviceInfo,
    });

// @ts-ignore
} else if (TARGET === 'webOS') {
    try {
        preloadData = {
            onStartupStorageClearCb: () => { localStorage.clear(); },
            onDeviceInfoCb: () => { console.log('Main: Callback not set while fetching device info'); },
            onConnectCb: (_, value: any) => { console.log('Main: Callback not set while calling onConnect'); },
            onDisconnectCb: (_, value: any) => { console.log('Main: Callback not set while calling onDisconnect'); },
        };

        window.targetAPI = {
            onStartupStorageClear: (callback: () => void) => preloadData.onStartupStorageClearCb = callback,
            onDeviceInfo: (callback: () => void) => preloadData.onDeviceInfoCb = callback,
            onConnect: (callback: (_, value: any) => void) => preloadData.onConnectCb = callback,
            onDisconnect: (callback: (_, value: any) => void) => preloadData.onDisconnectCb = callback,
            getDeviceInfo: () => preloadData.deviceInfo,
        };
    }
    catch (err) {
        console.error(`Main: preload ${JSON.stringify(err)}`);
        toast(`Main: preload ${JSON.stringify(err)}`, ToastIcon.ERROR);
    }
} else {
    // @ts-ignore
    console.log(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
