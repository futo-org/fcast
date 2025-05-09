/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/main/Preload';
import { toast, ToastIcon } from 'common/components/Toast';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');
const logger = window.targetAPI.logger;

enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

try {
    let getSessions = null;

    const toastService = requestService('toast', (message: any) => { toast(message.value.message, message.value.icon, message.value.duration); });
    const getDeviceInfoService = window.webOS.service.request('luna://com.palm.connectionmanager', {
        method: 'getStatus',
        parameters: {},
        onSuccess: (message: any) => {
            // logger.info('Network info status message', message);
            const deviceName = 'FCast-LGwebOSTV';
            const connections = [];

            if (message.wired.state !== 'disconnected') {
                connections.push({ type: 'wired', name: 'Ethernet', address: message.wired.ipAddress })
            }

            // wifiDirect never seems to be connected, despite being connected (which is needed for signalLevel...)
            // if (message.wifiDirect.state !== 'disconnected') {
            if (message.wifi.state !== 'disconnected') {
                connections.push({ type: 'wireless', name: message.wifi.ssid, address: message.wifi.ipAddress, signalLevel: 100 })
            }

            preloadData.deviceInfo = { name: deviceName, interfaces: connections };
            preloadData.onDeviceInfoCb();
        },
        onFailure: (message: any) => {
            logger.error(`Main: com.palm.connectionmanager/getStatus ${JSON.stringify(message)}`);
            toast(`Main: com.palm.connectionmanager/getStatus ${JSON.stringify(message)}`, ToastIcon.ERROR);

        },
        // onComplete: (message) => {},
        subscribe: true,
        resubscribe: true
    });

    window.targetAPI.getSessions(() => {
        return new Promise((resolve, reject) => {
            getSessions = requestService('get_sessions', (message: any) => resolve(message.value), (message: any) => reject(message), false);
        });
    });

    const onConnectService = requestService('connect', (message: any) => { preloadData.onConnectCb(null, message.value); });
    const onDisconnectService = requestService('disconnect', (message: any) => { preloadData.onDisconnectCb(null, message.value); });
    const playService = requestService('play', (message: any) => {
        if (message.value !== undefined && message.value.playData !== undefined) {
            logger.info(`Main: Playing ${JSON.stringify(message)}`);
            sessionStorage.setItem('playData', JSON.stringify(message.value.playData));
            getDeviceInfoService.cancel();
            getSessions?.cancel();
            toastService.cancel();
            onConnectService.cancel();
            onDisconnectService.cancel();
            playService.cancel();

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open('../player/index.html', '_self');
        }
     });

    const launchHandler = () => {
        const params = window.webOSDev.launchParams();
        logger.info(`Main: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        const lastTimestamp = Number(localStorage.getItem('lastTimestamp'));
        if (params.playData !== undefined && params.timestamp != lastTimestamp) {
            localStorage.setItem('lastTimestamp', params.timestamp);
            sessionStorage.setItem('playData', JSON.stringify(params.playData));
            toastService?.cancel();
            getDeviceInfoService?.cancel();
            getSessions?.cancel();
            onConnectService?.cancel();
            onDisconnectService?.cancel();
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
        // logger.info("KeyDown", event);

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
    logger.error(`Main: preload ${JSON.stringify(err)}`);
    toast(`Error starting the application (preload): ${JSON.stringify(err)}`, ToastIcon.ERROR);
}

function requestService(method: string, successCallback: (message: any) => void, failureCallback?: (message: any) => void, subscribe: boolean = true): any {
    const serviceId = 'com.futo.fcast.receiver.service';

    return window.webOS.service.request(`luna://${serviceId}/`, {
        method: method,
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value?.subscribed === true) {
                logger.info(`Main: Registered ${method} handler with service`);
            }
            else {
                successCallback(message);
            }
        },
        onFailure: (message: any) => {
            logger.error(`Main: ${method} ${JSON.stringify(message)}`);

            if (failureCallback) {
                failureCallback(message);
            }
        },
        // onComplete: (message) => {},
        subscribe: subscribe,
        resubscribe: subscribe
    });
}
