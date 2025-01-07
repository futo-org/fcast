/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-require-imports */
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

let deviceInfo: any;
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
        deviceInfo = value;
    })

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        onStartupStorageClear: (callback: any) => electronAPI.ipcRenderer.on('startup-storage-clear', callback),
        onDeviceInfo: (callback: any) => electronAPI.ipcRenderer.on("device-info", callback),
        onConnect: (callback: any) => electronAPI.ipcRenderer.on("connect", callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on("disconnect", callback),
        getDeviceInfo: () => deviceInfo,
    });

// @ts-ignore
} else if (TARGET === 'webOS') {
    try {
        require('lib/webOSTVjs-1.2.10/webOSTV.js');
        require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');
        const serviceId = 'com.futo.fcast.receiver.service';
        let onStartupStorageClearCb = () => { localStorage.clear(); };
        let onDeviceInfoCb = () => { console.log('Main: Callback not set while fetching device info'); };
        let onConnectCb = (_, value: any) => { console.log('Main: Callback not set while calling onConnect'); };
        let onDisconnectCb = (_, value: any) => { console.log('Main: Callback not set while calling onDisconnect'); };

        const getDeviceInfoService = window.webOS.service.request(`luna://${serviceId}/`, {
            method:"getDeviceInfo",
            parameters: {},
            onSuccess: (message: any) => {
                console.log(`Main: getDeviceInfo ${JSON.stringify(message)}`);

                deviceInfo = message.value;
                onDeviceInfoCb();
            },
            onFailure: (message: any) => {
                console.error(`Main: getDeviceInfo ${JSON.stringify(message)}`);
                toast(`Main: getDeviceInfo ${JSON.stringify(message)}`, ToastIcon.ERROR);
            },
            // onComplete: (message) => {},
        });

        window.targetAPI = {
            onStartupStorageClear: (callback: () => void) => onStartupStorageClearCb = callback,
            onDeviceInfo: (callback: () => void) => onDeviceInfoCb = callback,
            onConnect: (callback: () => void) => onConnectCb = callback,
            onDisconnect: (callback: () => void) => onDisconnectCb = callback,
            getDeviceInfo: () => deviceInfo,
        };

        preloadData = {
            getDeviceInfoService: getDeviceInfoService,
            onStartupStorageClearCb: onStartupStorageClearCb,
            onConnectCb: onConnectCb,
            onDisconnectCb: onDisconnectCb,
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
