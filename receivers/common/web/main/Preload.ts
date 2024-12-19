/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */

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

    electronAPI.ipcRenderer.on("device-info", (_event, value) => {
        deviceInfo = value;
    })

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        onDeviceInfo: (callback: any) => electronAPI.ipcRenderer.on("device-info", callback),
        getDeviceInfo: () => deviceInfo,
    });

// @ts-ignore
} else if (TARGET === 'webOS') {
    require('lib/webOSTVjs-1.2.10/webOSTV.js');
    require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');
    const serviceId = 'com.futo.fcast.receiver.service';
    let onDeviceInfoCb = () => { console.log('Main: Callback not set while fetching device info'); };

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
        },
        // onComplete: (message) => {},
    });

    window.targetAPI = {
        onDeviceInfo: (callback: () => void) => onDeviceInfoCb = callback,
        getDeviceInfo: () => deviceInfo,
    };

    preloadData = {
        getDeviceInfoService: getDeviceInfoService,
    };
} else {
    // @ts-ignore
    console.log(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
