/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
export {};

declare global {
    interface Window {
      electronAPI: any;
      webOS: any;
      webOSDev: any;
      targetAPI: any;
    }
}

let deviceInfo: any;

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
    let onDeviceInfoCb: any;

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

    const playService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"play",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Main: Registered play handler with service');
            }
            else {
                console.log(`Main: Playing ${JSON.stringify(message)}`);
                getDeviceInfoService.cancel();
                playService.cancel();
            }
        },
        onFailure: (message: any) => {
            console.error(`Main: play ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    window.targetAPI = {
        onDeviceInfo: (callback: any) => onDeviceInfoCb = callback,
        getDeviceInfo: () => deviceInfo,
    };

    document.addEventListener('webOSRelaunch', (args: any) => {
        console.log(`Relaunching FCast Receiver with args: ${JSON.stringify(args)}`);

        if (args.playData !== null) {
            if (getDeviceInfoService !== undefined) {
                getDeviceInfoService.cancel();
            }
            if (playService !== undefined) {
                playService.cancel();
            }

            window.open('../player/index.html');
        }
    });
} else {
    // @ts-ignore
    console.log(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}
