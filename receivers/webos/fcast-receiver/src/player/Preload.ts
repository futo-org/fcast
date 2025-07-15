/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/player/Preload';
import { EventMessage, PlaybackErrorMessage, PlaybackUpdateMessage, PlayMessage, VolumeUpdateMessage } from 'common/Packets';
import { ServiceManager, initializeWindowSizeStylesheet } from 'lib/common';
import { toast, ToastIcon } from 'common/components/Toast';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');

declare global {
    interface Window {
        targetAPI: any;
        webOSAPI: any;
        webOSApp: any;
    }
}

const logger = window.targetAPI.logger;

try {
    initializeWindowSizeStylesheet();

    window.webOSAPI = {
        pendingPlay: JSON.parse(sessionStorage.getItem('playInfo'))
    };
    const contentViewer = window.webOSAPI.pendingPlay?.contentViewer;

    const serviceManager: ServiceManager = window.parent.webOSApp.serviceManager;
    serviceManager.subscribeToServiceChannel((message: any) => {
        switch (message.event) {
            case 'toast':
                preloadData.onToastCb(message.value.message, message.value.icon, message.value.duration);
                break;

            case 'play': {
                if (contentViewer !== message.value.contentViewer) {
                    window.parent.webOSApp.loadPage(`${message.value.contentViewer}/index.html`);
                }
                else {
                    if (message.value.rendererEvent === 'play-playlist') {
                        if (preloadData.onPlayCb === undefined) {
                            window.webOSAPI.pendingPlay = message.value;
                        }
                        else {
                            preloadData.onPlayPlaylistCb(null, message.value.rendererMessage);
                        }
                    }
                    else {
                        if (preloadData.onPlayCb === undefined) {
                            window.webOSAPI.pendingPlay = message.value;
                        }
                        else {
                            preloadData.onPlayCb(null, message.value.rendererMessage);
                        }
                    }
                }
                break;
            }

            case 'pause':
                preloadData.onPauseCb();
                break;

            case 'resume':
                preloadData.onResumeCb();
                break;

            case 'stop':
                window.parent.webOSApp.loadPage('main_window/index.html');
                break;

            case 'seek':
                preloadData.onSeekCb(null, message.value);
                break;

            case 'setvolume':
                preloadData.onSetVolumeCb(null, message.value);
                break;

            case 'setspeed':
                preloadData.onSetSpeedCb(null, message.value);
                break;

            case 'setplaylistitem':
                preloadData.onSetPlaylistItemCb(null, message.value);
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

            // 'play-playlist' is handled in the 'play' message for webOS

            default:
                break;
        }
    });

    preloadData.sendPlaybackErrorCb = (error: PlaybackErrorMessage) => {
        serviceManager.call('send_playback_error', error, null, (message: any) => { logger.error(`Player: send_playback_error ${JSON.stringify(message)}`); });
    };
    preloadData.sendPlaybackUpdateCb = (update: PlaybackUpdateMessage) => {
        serviceManager.call('send_playback_update', update, null, (message: any) => { logger.error(`Player: send_playback_update ${JSON.stringify(message)}`); });
    };
    preloadData.sendVolumeUpdateCb = (update: VolumeUpdateMessage) => {
        serviceManager.call('send_volume_update', update, null, (message: any) => { logger.error(`Player: send_volume_update ${JSON.stringify(message)}`); });
    };
    preloadData.sendEventCb = (event: EventMessage) => {
        serviceManager.call('send_event', event, null, (message: any) => { logger.error(`Player: send_event ${JSON.stringify(message)}`); });
    };

    preloadData.sendPlayRequestCb = (message: PlayMessage, playlistIndex: number) => {
        serviceManager.call('play_request', { message: message, playlistIndex: playlistIndex }, null, (message: any) => { logger.error(`Player: play_request ${playlistIndex} ${JSON.stringify(message)}`); });
    };
    window.targetAPI.getSessions(() => {
        return new Promise((resolve, reject) => {
            serviceManager.call('get_sessions', {}, (message: any) => resolve(message.value), (message: any) => reject(message));
        });
    });

    const launchHandler = () => {
        // args don't seem to be passed in via event despite what documentation says...
        const params = window.webOSDev.launchParams();
        logger.info(`Player: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        // WebOS 6.0 and earlier: Timestamp tracking seems to be necessary as launch event is raised regardless if app is in foreground or not
        const lastTimestamp = Number(sessionStorage.getItem('lastTimestamp'));
        if (params.messageInfo !== undefined && params.timestamp != lastTimestamp) {
            sessionStorage.setItem('lastTimestamp', params.timestamp);
            sessionStorage.setItem('playInfo', JSON.stringify(params.messageInfo));

            window.parent.webOSApp.loadPage(`${params.messageInfo.contentViewer}/index.html`);
        }
    };

    window.parent.webOSApp.setLaunchHandler(launchHandler);
    document.addEventListener('visibilitychange', () => serviceManager.call('visibility_changed', { hidden: document.hidden, window: contentViewer }));
}
catch (err) {
    logger.error(`Player: preload`, err);
    toast(`Error starting the video player (preload): ${JSON.stringify(err)}`, ToastIcon.ERROR);
}
