/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/main/Preload';
import { toast, ToastIcon } from 'common/components/Toast';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');

enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

try {
    const toastService = registerService('toast', (message: any) => { toast(message.value.message, message.value.icon, message.value.duration); });
    const getDeviceInfoService = registerService('getDeviceInfo', (message: any) => {
        console.log(`Main: getDeviceInfo ${JSON.stringify(message)}`);
        preloadData.deviceInfo = message.value;
        preloadData.onDeviceInfoCb();
    }, false);
    const onConnectService = registerService('connect', (message: any) => { preloadData.onConnectCb(null, message.value); });
    const onDisconnectService = registerService('disconnect', (message: any) => { preloadData.onDisconnectCb(null, message.value); });
    const onPingService = registerService('ping', (message: any) => { preloadData.onPingCb(null, message.value); });
    const playService = registerService('play', (message: any) => {
        if (message.value !== undefined && message.value.playData !== undefined) {
            console.log(`Main: Playing ${JSON.stringify(message)}`);
            sessionStorage.setItem('playData', JSON.stringify(message.value.playData));
            getDeviceInfoService.cancel();
            toastService.cancel();
            onConnectService.cancel();
            onDisconnectService.cancel();
            onPingService.cancel();
            playService.cancel();

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open('../player/index.html', '_self');
        }
     });

    const launchHandler = () => {
        const params = window.webOSDev.launchParams();
        console.log(`Main: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        const lastTimestamp = Number(localStorage.getItem('lastTimestamp'));
        if (params.playData !== undefined && params.timestamp != lastTimestamp) {
            localStorage.setItem('lastTimestamp', params.timestamp);
            sessionStorage.setItem('playData', JSON.stringify(params.playData));
            toastService?.cancel();
            getDeviceInfoService?.cancel();
            onConnectService?.cancel();
            onDisconnectService?.cancel();
            onPingService?.cancel();
            playService?.cancel();

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open('../player/index.html', '_self');
        }
    };

    document.addEventListener('webOSLaunch', launchHandler);
    document.addEventListener('webOSRelaunch', launchHandler);

    // Cannot go back to a state where user was previously casting a video, so exit.
    // window.onpopstate = () => {
    //     window.webOS.platformBack();
    // };

    document.addEventListener('keydown', (event: any) => {
        // console.log("KeyDown", event);

        switch (event.keyCode) {
            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            case RemoteKeyCode.Back:
                window.webOS.platformBack();
                break;
            default:
                break;
        }
    });
}
catch (err) {
    console.error(`Main: preload ${JSON.stringify(err)}`);
    toast(`Main: preload ${JSON.stringify(err)}`, ToastIcon.ERROR);
}

function registerService(method: string, callback: (message: any) => void, subscribe: boolean = true): any {
    const serviceId = 'com.futo.fcast.receiver.service';

    return window.webOS.service.request(`luna://${serviceId}/`, {
        method: method,
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value?.subscribed === true) {
                console.log(`Main: Registered ${method} handler with service`);
            }
            else {
                callback(message);
            }
        },
        onFailure: (message: any) => {
            console.error(`Main: ${method} ${JSON.stringify(message)}`);
            toast(`Main: ${method} ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        // onComplete: (message) => {},
        subscribe: subscribe,
        resubscribe: subscribe
    });
}
