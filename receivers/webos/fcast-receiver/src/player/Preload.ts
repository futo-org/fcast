/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/player/Preload';
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from 'common/Packets';
import { toast, ToastIcon } from 'common/components/Toast';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');
const logger = window.targetAPI.logger;

try {
    const serviceId = 'com.futo.fcast.receiver.service';
    let getSessions = null;

    window.webOSAPI = {
        pendingPlay: JSON.parse(sessionStorage.getItem('playData'))
    };

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

    const playService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"play",
        parameters: {},
        onSuccess: (message: any) => {
            // logger.info(JSON.stringify(message));
            if (message.value.subscribed === true) {
                logger.info('Player: Registered play handler with service');
            }

            if (message.value.playData !== null) {
                if (preloadData.onPlayCb === undefined) {
                    window.webOSAPI.pendingPlay = message.value.playData;
                }
                else {
                    preloadData.onPlayCb(null, message.value.playData);
                }
            }
        },
        onFailure: (message: any) => {
            logger.error(`Player: play ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const pauseService = requestService('pause', () => { preloadData.onPauseCb(); });
    const resumeService = requestService('resume', () => { preloadData.onResumeCb(); });
    const stopService = requestService('stop', () => {
        playService.cancel();
        pauseService.cancel();
        resumeService.cancel();
        stopService.cancel();
        seekService.cancel();
        setVolumeService.cancel();
        setSpeedService.cancel();
        getSessions?.cancel();
        onConnectService.cancel();
        onDisconnectService.cancel();

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        // history.back();
        window.open('../main_window/index.html', '_self');
     });

    const seekService = requestService('seek', (message: any) => { preloadData.onSeekCb(null, message.value); });
    const setVolumeService = requestService('setvolume', (message: any) => { preloadData.onSetVolumeCb(null, message.value); });
    const setSpeedService = requestService('setspeed', (message: any) => { preloadData.onSetSpeedCb(null, message.value); });

    window.targetAPI.getSessions(() => {
        return new Promise((resolve, reject) => {
            getSessions = requestService('get_sessions', (message: any) => resolve(message.value), (message: any) => reject(message), false);
        });
    });

    const onConnectService = requestService('connect', (message: any) => { preloadData.onConnectCb(null, message.value); });
    const onDisconnectService = requestService('disconnect', (message: any) => { preloadData.onDisconnectCb(null, message.value); });

    const launchHandler = () => {
        // args don't seem to be passed in via event despite what documentation says...
        const params = window.webOSDev.launchParams();
        logger.info(`Player: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        const lastTimestamp = Number(localStorage.getItem('lastTimestamp'));
        if (params.playData !== undefined && params.timestamp != lastTimestamp) {
            localStorage.setItem('lastTimestamp', params.timestamp);
            sessionStorage.setItem('playData', JSON.stringify(params.playData));
            playService?.cancel();
            pauseService?.cancel();
            resumeService?.cancel();
            stopService?.cancel();
            seekService?.cancel();
            setVolumeService?.cancel();
            setSpeedService?.cancel();
            getSessions?.cancel();
            onConnectService?.cancel();
            onDisconnectService?.cancel();

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open('../player/index.html', '_self');
        }
    };

    document.addEventListener('webOSLaunch', launchHandler);
    document.addEventListener('webOSRelaunch', launchHandler);

}
catch (err) {
    logger.error(`Player: preload ${JSON.stringify(err)}`);
    toast(`Error starting the video player (preload): ${JSON.stringify(err)}`, ToastIcon.ERROR);
}

function requestService(method: string, successCallback: (message: any) => void, failureCallback?: (message: any) => void, subscribe: boolean = true): any {
    const serviceId = 'com.futo.fcast.receiver.service';

    return window.webOS.service.request(`luna://${serviceId}/`, {
        method: method,
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value?.subscribed === true) {
                logger.info(`Player: Registered ${method} handler with service`);
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
