/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/player/Preload';
import { EventMessage, PlaybackErrorMessage, PlaybackUpdateMessage, PlayMessage, VolumeUpdateMessage } from 'common/Packets';
import { callService, requestService } from 'lib/common';
import { toast, ToastIcon } from 'common/components/Toast';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');
const logger = window.targetAPI.logger;
const serviceId = 'com.futo.fcast.receiver.service';

try {
    let getSessions = null;

    window.webOSAPI = {
        pendingPlay: JSON.parse(sessionStorage.getItem('playInfo'))
    };
    const contentViewer = window.webOSAPI.pendingPlay?.contentViewer;

    preloadData.sendPlaybackErrorCb = (error: PlaybackErrorMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_playback_error',
            parameters: { error },
            onSuccess: () => {},
            onFailure: (message: any) => {
                logger.error(`Player: send_playback_error ${JSON.stringify(message)}`);
            },
        });
    };
    preloadData.sendPlaybackUpdateCb = (update: PlaybackUpdateMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_playback_update',
            parameters: { update },
            // onSuccess: (message: any) => {
            //     logger.info(`Player: send_playback_update ${JSON.stringify(message)}`);
            // },
            onSuccess: () => {},
            onFailure: (message: any) => {
                logger.error(`Player: send_playback_update ${JSON.stringify(message)}`);
            },
        });
    };
    preloadData.sendVolumeUpdateCb = (update: VolumeUpdateMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_volume_update',
            parameters: { update },
            onSuccess: () => {},
            onFailure: (message: any) => {
                logger.error(`Player: send_volume_update ${JSON.stringify(message)}`);
            },
        });
    };
    preloadData.sendEventCb = (event: EventMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_event',
            parameters: { event },
            onSuccess: () => {},
            onFailure: (message: any) => { logger.error(`Player: send_event ${JSON.stringify(message)}`); },
        });
    };

    const playService = requestService('play', (message: any) => {
        if (contentViewer !== message.value.contentViewer) {
            playService?.cancel();
            pauseService?.cancel();
            resumeService?.cancel();
            stopService?.cancel();
            seekService?.cancel();
            setVolumeService?.cancel();
            setSpeedService?.cancel();
            onSetPlaylistItemService?.cancel();
            getSessions?.cancel();
            onEventSubscribedKeysUpdateService?.cancel();
            onConnectService?.cancel();
            onDisconnectService?.cancel();
            onPlayPlaylistService?.cancel();

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open(`../${message.value.contentViewer}/index.html`, '_self');
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
    }, (message: any) => {
        logger.error(`Player: play ${JSON.stringify(message)}`);
    });
    const pauseService = requestService('pause', () => { preloadData.onPauseCb(); });
    const resumeService = requestService('resume', () => { preloadData.onResumeCb(); });
    const stopService = requestService('stop', () => {
        playService?.cancel();
        pauseService?.cancel();
        resumeService?.cancel();
        stopService?.cancel();
        seekService?.cancel();
        setVolumeService?.cancel();
        setSpeedService?.cancel();
        onSetPlaylistItemService?.cancel();
        getSessions?.cancel();
        onEventSubscribedKeysUpdateService?.cancel();
        onConnectService?.cancel();
        onDisconnectService?.cancel();
        onPlayPlaylistService?.cancel();

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        // history.back();
        window.open('../main_window/index.html', '_self');
     });

    const seekService = requestService('seek', (message: any) => { preloadData.onSeekCb(null, message.value); });
    const setVolumeService = requestService('setvolume', (message: any) => { preloadData.onSetVolumeCb(null, message.value); });
    const setSpeedService = requestService('setspeed', (message: any) => { preloadData.onSetSpeedCb(null, message.value); });
    const onSetPlaylistItemService = requestService('setplaylistitem', (message: any) => { preloadData.onSetPlaylistItemCb(null, message.value); });

    preloadData.sendPlayRequestCb = (message: PlayMessage, playlistIndex: number) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'play_request',
            parameters: { message: message, playlistIndex: playlistIndex },
            onSuccess: () => {},
            onFailure: (message: any) => { logger.error(`Player: play_request ${playlistIndex} ${JSON.stringify(message)}`); },
        });
    };
    window.targetAPI.getSessions(() => {
        return new Promise((resolve, reject) => {
            getSessions = callService('get_sessions', {}, (message: any) => resolve(message.value), (message: any) => reject(message));
        });
    });

    const onEventSubscribedKeysUpdateService = requestService('event_subscribed_keys_update', (message: any) => { preloadData.onEventSubscribedKeysUpdate(message.value); });
    const onConnectService = requestService('connect', (message: any) => { preloadData.onConnectCb(null, message.value); });
    const onDisconnectService = requestService('disconnect', (message: any) => { preloadData.onDisconnectCb(null, message.value); });
    const onPlayPlaylistService = requestService('play-playlist', (message: any) => { preloadData.onPlayPlaylistCb(null, message.value); });

    const launchHandler = () => {
        // args don't seem to be passed in via event despite what documentation says...
        const params = window.webOSDev.launchParams();
        logger.info(`Player: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        // WebOS 6.0 and earlier: Timestamp tracking seems to be necessary as launch event is raised regardless if app is in foreground or not
        const lastTimestamp = Number(localStorage.getItem('lastTimestamp'));
        if (params.messageInfo !== undefined && params.timestamp != lastTimestamp) {
            localStorage.setItem('lastTimestamp', params.timestamp);
            sessionStorage.setItem('playInfo', JSON.stringify(params.messageInfo));

            playService?.cancel();
            pauseService?.cancel();
            resumeService?.cancel();
            stopService?.cancel();
            seekService?.cancel();
            setVolumeService?.cancel();
            setSpeedService?.cancel();
            onSetPlaylistItemService?.cancel();
            getSessions?.cancel();
            onEventSubscribedKeysUpdateService?.cancel();
            onConnectService?.cancel();
            onDisconnectService?.cancel();
            onPlayPlaylistService?.cancel();

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open(`../${params.messageInfo.contentViewer}/index.html`, '_self');
        }
    };

    document.addEventListener('webOSLaunch', launchHandler);
    document.addEventListener('webOSRelaunch', launchHandler);
    document.addEventListener('visibilitychange', () => callService('visibility_changed', { hidden: document.hidden, window: contentViewer }));

}
catch (err) {
    logger.error(`Player: preload ${JSON.stringify(err)}`);
    toast(`Error starting the video player (preload): ${JSON.stringify(err)}`, ToastIcon.ERROR);
}
