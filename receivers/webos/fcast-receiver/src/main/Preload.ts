/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/main/Preload';
import { ToastIcon } from 'common/components/Toast';
import { EventMessage } from 'common/Packets';
import { callService, requestService } from 'lib/common';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');
const logger = window.targetAPI.logger;

try {
    const serviceId = 'com.futo.fcast.receiver.service';
    let getSessionsService = null;
    let networkChangedService = null;
    let visibilityChangedService = null;

    const toastService = requestService('toast', (message: any) => { preloadData.onToastCb(message.value.message, message.value.icon, message.value.duration); });
    const getDeviceInfoService = window.webOSDev.connection.getStatus({
        onSuccess: (message: any) => {
            logger.info('Network info status message', message);
            const deviceName = 'FCast-LGwebOSTV';
            const connections = [];
            let fallback = true;

            if (message.wired.state !== 'disconnected') {
                connections.push({ type: 'wired', name: 'Ethernet', address: message.wired.ipAddress });
                fallback = false;
            }

            // wifiDirect never seems to be connected, despite being connected (which is needed for signalLevel...)
            // if (message.wifiDirect.state !== 'disconnected') {
            if (message.wifi.state !== 'disconnected') {
                connections.push({ type: 'wireless', name: message.wifi.ssid, address: message.wifi.ipAddress, signalLevel: 100 });
                fallback = false;
            }

            if (fallback) {
                networkChangedService = callService('network_changed', { fallback: fallback }, (message: any) => {
                    logger.info('Fallback network interfaces', message);
                    for (const ipAddr of message.value) {
                        connections.push({ type: 'wired', name: 'Ethernet', address: ipAddr });
                    }

                    preloadData.deviceInfo = { name: deviceName, interfaces: connections };
                    preloadData.onDeviceInfoCb();
                }, (message: any) => {
                    logger.error('Main: preload - error fetching network interfaces', message);
                    preloadData.onToastCb('Error detecting network interfaces', ToastIcon.ERROR);
                }, () => {
                    networkChangedService = null;
                });
            }
            else {
                networkChangedService = callService('network_changed', { fallback: fallback }, null, null, () => {
                    networkChangedService = null;
                });
                preloadData.deviceInfo = { name: deviceName, interfaces: connections };
                preloadData.onDeviceInfoCb();
            }
        },
        onFailure: (message: any) => {
            logger.error(`Main: com.palm.connectionmanager/getStatus ${JSON.stringify(message)}`);
            preloadData.onToastCb(`Main: com.palm.connectionmanager/getStatus ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        subscribe: true,
        resubscribe: true
    });

    const onEventSubscribedKeysUpdateService = requestService('event_subscribed_keys_update', (message: any) => { preloadData.onEventSubscribedKeysUpdate(message.value); });
    window.targetAPI.getSessions(() => {
        return new Promise((resolve, reject) => {
            getSessionsService = callService('get_sessions', {}, (message: any) => resolve(message.value), (message: any) => reject(message));
        });
    });

    const onConnectService = requestService('connect', (message: any) => { preloadData.onConnectCb(null, message.value); });
    const onDisconnectService = requestService('disconnect', (message: any) => { preloadData.onDisconnectCb(null, message.value); });
    preloadData.sendEventCb = (event: EventMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_event',
            parameters: { event },
            onSuccess: () => {},
            onFailure: (message: any) => { logger.error(`Player: send_event ${JSON.stringify(message)}`); },
        });
    };

    const playService = requestService('play', (message: any) => {
        logger.info(`Main: Playing ${JSON.stringify(message)}`);
        play(message.value);
     });

    const launchHandler = () => {
        const params = window.webOSDev.launchParams();
        logger.info(`Main: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        // WebOS 6.0 and earlier: Timestamp tracking seems to be necessary as launch event is raised regardless if app is in foreground or not
        const lastTimestamp = Number(localStorage.getItem('lastTimestamp'));
        if (params.messageInfo !== undefined && params.timestamp != lastTimestamp) {
            localStorage.setItem('lastTimestamp', params.timestamp);
            play(params.messageInfo);
        }
    };

    document.addEventListener('webOSLaunch', launchHandler);
    document.addEventListener('webOSRelaunch', launchHandler);
    document.addEventListener('visibilitychange', () => {
        visibilityChangedService = callService('visibility_changed', { hidden: document.hidden, window: 'main' }, null, null, () => {
            visibilityChangedService = null;
        })
    });

    // Cannot go back to a state where user was previously casting a video, so exit.
    // window.onpopstate = () => {
    //     window.webOS.platformBack();
    // };

    const play = (messageInfo: any) => {
        sessionStorage.setItem('playInfo', JSON.stringify(messageInfo));

        getDeviceInfoService?.cancel();
        onEventSubscribedKeysUpdateService?.cancel();
        getSessionsService?.cancel();
        toastService?.cancel();
        onConnectService?.cancel();
        onDisconnectService?.cancel();
        playService?.cancel();
        networkChangedService?.cancel();
        visibilityChangedService?.cancel();

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        // history.pushState({}, '', '../main_window/index.html');
        window.open(`../${messageInfo.contentViewer}/index.html`, '_self');
    };
}
catch (err) {
    logger.error(`Main: preload ${JSON.stringify(err)}`);
    preloadData.onToastCb(`Error starting the application: ${JSON.stringify(err)}`, ToastIcon.ERROR);
}
