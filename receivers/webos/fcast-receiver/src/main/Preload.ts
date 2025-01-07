/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/main/Preload';
import { toast, ToastIcon } from 'common/components/Toast';

enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

try {
    const serviceId = 'com.futo.fcast.receiver.service';

    const startupStorageClearService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"startup-storage-clear",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Main: Registered startup-storage-clear handler with service');
            }
            else {
                preloadData.onStartupStorageClearCb();
            }
        },
        onFailure: (message: any) => {
            console.error(`Main: startup-storage-clear ${JSON.stringify(message)}`);
            toast(`Main: startup-storage-clear ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        subscribe: true,
        resubscribe: true
    });


    const toastService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"toast",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Main: Registered toast handler with service');
            }
            else {
                toast(message.value.message, message.value.icon, message.value.duration);
            }
        },
        onFailure: (message: any) => {
            console.error(`Main: toast ${JSON.stringify(message)}`);
            toast(`Main: toast ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        subscribe: true,
        resubscribe: true
    });

    const onConnectService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"connect",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Main: Registered connect handler with service');
            }
            else {
                preloadData.onConnectCb(null, message.value);
            }
        },
        onFailure: (message: any) => {
            console.error(`Main: connect ${JSON.stringify(message)}`);
            toast(`Main: connect ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        subscribe: true,
        resubscribe: true
    });

    const onDisconnectService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"disconnect",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Main: Registered disconnect handler with service');
            }
            else {
                preloadData.onDisconnectCb(null, message.value);
            }
        },
        onFailure: (message: any) => {
            console.error(`Main: disconnect ${JSON.stringify(message)}`);
            toast(`Main: disconnect ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        subscribe: true,
        resubscribe: true
    });

    const playService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"play",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Main: Registered play handler with service');
            }
            else {
                if (message.value !== undefined && message.value.playData !== undefined) {
                    console.log(`Main: Playing ${JSON.stringify(message)}`);
                    preloadData.getDeviceInfoService.cancel();
                    startupStorageClearService.cancel();
                    toastService.cancel();
                    onConnectService.cancel();
                    onDisconnectService.cancel();
                    playService.cancel();

                    // WebOS 22 and earlier does not work well using the history API,
                    // so manually handling page navigation...
                    // history.pushState({}, '', '../main_window/index.html');
                    window.open('../player/index.html');
                }
            }
        },
        onFailure: (message: any) => {
            console.error(`Main: play ${JSON.stringify(message)}`);
            toast(`Main: play ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        subscribe: true,
        resubscribe: true
    });

    const launchHandler = (args: any) => {
        // args don't seem to be passed in via event despite what documentation says...
        const params = window.webOSDev.launchParams();
        console.log(`Main: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        const lastTimestamp = localStorage.getItem('lastTimestamp');
        if (params.playData !== undefined && params.timestamp != lastTimestamp) {
            localStorage.setItem('lastTimestamp', params.timestamp);
            if (preloadData.getDeviceInfoService !== undefined) {
                preloadData.getDeviceInfoService.cancel();
            }
            if (startupStorageClearService !== undefined) {
                startupStorageClearService.cancel();
            }
            if (toastService !== undefined) {
                toastService.cancel();
            }
            if (onConnectService !== undefined) {
                onConnectService.cancel();
            }
            if (onDisconnectService !== undefined) {
                onDisconnectService.cancel();
            }
            if (playService !== undefined) {
                playService.cancel();
            }

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open('../player/index.html');
        }
    };

    document.addEventListener('webOSLaunch', (ags) => { launchHandler(ags)});
    document.addEventListener('webOSRelaunch', (ags) => { launchHandler(ags)});

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
