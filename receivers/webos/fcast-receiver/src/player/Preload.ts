/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { preloadData } from 'common/player/Preload';
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from 'common/Packets';
import { toast, ToastIcon } from 'common/components/Toast';
require('lib/webOSTVjs-1.2.10/webOSTV.js');
require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');

try {
    const serviceId = 'com.futo.fcast.receiver.service';
    let playerWindowOpen = false;

    window.webOSAPI = {
        pendingPlay: null
    };

    preloadData.sendPlaybackErrorCb = (error: PlaybackErrorMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_playback_error',
            parameters: { error },
            onSuccess: () => {},
            onFailure: (message: any) => {
                console.error(`Player: send_playback_error ${JSON.stringify(message)}`);
            },
        });
    };
    preloadData.sendPlaybackUpdateCb = (update: PlaybackUpdateMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_playback_update',
            parameters: { update },
            // onSuccess: (message: any) => {
            //     console.log(`Player: send_playback_update ${JSON.stringify(message)}`);
            // },
            onSuccess: () => {},
            onFailure: (message: any) => {
                console.error(`Player: send_playback_update ${JSON.stringify(message)}`);
            },
        });
    };
    preloadData.sendVolumeUpdateCb = (update: VolumeUpdateMessage) => {
        window.webOS.service.request(`luna://${serviceId}/`, {
            method: 'send_volume_update',
            parameters: { update },
            onSuccess: () => {},
            onFailure: (message: any) => {
                console.error(`Player: send_volume_update ${JSON.stringify(message)}`);
            },
        });
    };

    const playService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"play",
        parameters: {},
        onSuccess: (message: any) => {
            // console.log(JSON.stringify(message));
            if (message.value.subscribed === true) {
                console.log('Player: Registered play handler with service');
            }

            if (message.value.playData !== null) {
                if (!playerWindowOpen) {
                    playerWindowOpen = true;
                }

                if (preloadData.onPlayCb === undefined) {
                    window.webOSAPI.pendingPlay = message.value.playData;
                }
                else {
                    preloadData.onPlayCb(null, message.value.playData);
                }
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: play ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const pauseService = registerService('pause', () => { preloadData.onPauseCb(); });
    const resumeService = registerService('resume', () => { preloadData.onResumeCb(); });
    const stopService = registerService('stop', () => {
        playerWindowOpen = false;
        playService.cancel();
        pauseService.cancel();
        resumeService.cancel();
        stopService.cancel();
        seekService.cancel();
        setVolumeService.cancel();
        setSpeedService.cancel();

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        // history.back();
        window.open('../main_window/index.html');
     });

    const seekService = registerService('seek', (message: any) => { preloadData.onSeekCb(null, message.value); });
    const setVolumeService = registerService('setvolume', (message: any) => { preloadData.onSetVolumeCb(null, message.value); });
    const setSpeedService = registerService('setspeed', (message: any) => { preloadData.onSetSpeedCb(null, message.value); });

    const launchHandler = (args: any) => {
        // args don't seem to be passed in via event despite what documentation says...
        const params = window.webOSDev.launchParams();
        console.log(`Player: (Re)launching FCast Receiver with args: ${JSON.stringify(params)}`);

        const lastTimestamp = localStorage.getItem('lastTimestamp');
        if (params.playData !== undefined && params.timestamp != lastTimestamp) {
            localStorage.setItem('lastTimestamp', params.timestamp);
            playerWindowOpen = false;

            playService?.cancel();
            pauseService?.cancel();
            resumeService?.cancel();
            stopService?.cancel();
            seekService?.cancel();
            setVolumeService?.cancel();
            setSpeedService?.cancel();

            // WebOS 22 and earlier does not work well using the history API,
            // so manually handling page navigation...
            // history.pushState({}, '', '../main_window/index.html');
            window.open('../player/index.html');
        }
    };

    document.addEventListener('webOSLaunch', (ags) => { launchHandler(ags)});
    document.addEventListener('webOSRelaunch', (ags) => { launchHandler(ags)});

}
catch (err) {
    console.error(`Player: preload ${JSON.stringify(err)}`);
    toast(`Player: preload ${JSON.stringify(err)}`, ToastIcon.ERROR);
}

function registerService(method: string, callback: (message: any) => void, subscribe: boolean = true): any {
    const serviceId = 'com.futo.fcast.receiver.service';

    return window.webOS.service.request(`luna://${serviceId}/`, {
        method: method,
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log(`Player: Registered ${method} handler with service`);
            }
            else {
                callback(message);
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: ${method} ${JSON.stringify(message)}`);
            // toast(`Player: ${method} ${JSON.stringify(message)}`, ToastIcon.ERROR);
        },
        // onComplete: (message) => {},
        subscribe: subscribe,
        resubscribe: subscribe
    });
}
