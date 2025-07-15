/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/main/Preload';
import { ToastIcon } from 'common/components/Toast';
import { EventMessage } from 'common/Packets';
import { ServiceManager, initializeWindowSizeStylesheet } from 'lib/common';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');

declare global {
    interface Window {
        targetAPI: any;
        webOSApp: any;
    }
}

const logger = window.targetAPI.logger;

try {
    initializeWindowSizeStylesheet();

    const serviceManager: ServiceManager = window.parent.webOSApp.serviceManager;
    serviceManager.subscribeToServiceChannel((message: any) => {
        switch (message.event) {
            case 'toast':
                preloadData.onToastCb(message.value.message, message.value.icon, message.value.duration);
                break;

            case 'event_subscribed_keys_update':
                preloadData.onEventSubscribedKeysUpdate(message.value);
                break;

            case 'connect':
                preloadData.onConnectCb(null, message.value);
                break;

            case 'disconnect':
                preloadData.onDisconnectCb(null, message.value);
                break;

            case 'play':
                logger.info(`Main: Playing ${JSON.stringify(message)}`);
                play(message.value);
                break;

            default:
                break;
        }
    });

    const getDeviceInfoService = window.webOSDev.connection.getStatus({
        onSuccess: (message: any) => {
            logger.info('Network info status message', message);
            const deviceName = 'FCast-LGwebOSTV';
            const connections: any[] = [];
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
                const ipsIfaceName = document.getElementById('ips-iface-name');
                ipsIfaceName.style.display = 'none';

                serviceManager.call('network_changed', { fallback: fallback }, (message: any) => {
                    logger.info('Fallback network interfaces', message);
                    for (const ipAddr of message.value) {
                        connections.push({ type: 'wired', name: 'Ethernet', address: ipAddr });
                    }

                    preloadData.deviceInfo = { name: deviceName, interfaces: connections };
                    preloadData.onDeviceInfoCb();
                }, (message: any) => {
                    logger.error('Main: preload - error fetching network interfaces', message);
                    preloadData.onToastCb('Error detecting network interfaces', ToastIcon.ERROR);
                });
            }
            else {
                serviceManager.call('network_changed', { fallback: fallback });
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

    window.targetAPI.getSessions(() => {
        return new Promise((resolve, reject) => {
            serviceManager.call('get_sessions', {}, (message: any) => resolve(message.value), (message: any) => reject(message));
        });
    });

    preloadData.sendEventCb = (event: EventMessage) => {
        serviceManager.call('send_event', event, null, (message: any) => { logger.error(`Player: send_event ${JSON.stringify(message)}`); });
    };

    const launchHandler = () => {
        const params = window.webOSDev.launchParams();
        logger.info(`Main: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        // WebOS 6.0 and earlier: Timestamp tracking seems to be necessary as launch event is raised regardless if app is in foreground or not
        const lastTimestamp = Number(sessionStorage.getItem('lastTimestamp'));
        if (params.messageInfo !== undefined && params.timestamp != lastTimestamp) {
            sessionStorage.setItem('lastTimestamp', params.timestamp);
            play(params.messageInfo);
        }
    };

    window.parent.webOSApp.setLaunchHandler(launchHandler);
    document.addEventListener('visibilitychange', () => { serviceManager.call('visibility_changed', { hidden: document.hidden, window: 'main' }); });

    const play = (messageInfo: any) => {
        sessionStorage.setItem('playInfo', JSON.stringify(messageInfo));
        getDeviceInfoService?.cancel();

        window.parent.webOSApp.loadPage(`${messageInfo.contentViewer}/index.html`);
    };
}
catch (err) {
    logger.error(`Main: preload`, err);
    preloadData.onToastCb(`Error starting the application: ${JSON.stringify(err)}`, ToastIcon.ERROR);
}
