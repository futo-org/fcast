/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-require-imports */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from 'common/Packets';
export {};

declare global {
    interface Window {
      electronAPI: any;
      webOSAPI: any;
      webOS: any;
      targetAPI: any;
    }
}

// @ts-ignore
if (TARGET === 'electron') {
    // @ts-ignore
    const electronAPI = __non_webpack_require__('electron');

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        sendPlaybackError: (error: PlaybackErrorMessage) => electronAPI.ipcRenderer.send('send-playback-error', error),
        sendPlaybackUpdate: (update: PlaybackUpdateMessage) => electronAPI.ipcRenderer.send('send-playback-update', update),
        sendVolumeUpdate: (update: VolumeUpdateMessage) => electronAPI.ipcRenderer.send('send-volume-update', update),
        onPlay: (callback: any) => electronAPI.ipcRenderer.on("play", callback),
        onPause: (callback: any) => electronAPI.ipcRenderer.on("pause", callback),
        onResume: (callback: any) => electronAPI.ipcRenderer.on("resume", callback),
        onSeek: (callback: any) => electronAPI.ipcRenderer.on("seek", callback),
        onSetVolume: (callback: any) => electronAPI.ipcRenderer.on("setvolume", callback),
        onSetSpeed: (callback: any) => electronAPI.ipcRenderer.on("setspeed", callback)
    });

// @ts-ignore
} else if (TARGET === 'webOS') {
    require('lib/webOSTVjs-1.2.10/webOSTV.js');
    require('lib/webOSTVjs-1.2.10/webOSTV-dev.js');

    const serviceId = 'com.futo.fcast.receiver.service';
    let onPlayCb, onPauseCb, onResumeCb;
    let onSeekCb, onSetVolumeCb, onSetSpeedCb;
    let playerWindowOpen = false;

    const keepAliveService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"keepAlive",
        parameters: {},
        onSuccess: (message: any) => {
            console.log(`Player: keepAlive ${JSON.stringify(message)}`);
        },
        onFailure: (message: any) => {
            console.error(`Player: keepAlive ${JSON.stringify(message)}`);
        },
        // onComplete: (message) => {},
    });

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

                if (onPlayCb === undefined) {
                    window.webOSAPI.pendingPlay = message.value.playData;
                }
                else {
                    onPlayCb(null, message.value.playData);
                }
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: play ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const pauseService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"pause",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Player: Registered pause handler with service');
            }
            else {
                onPauseCb();
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: pause ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const resumeService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"resume",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Player: Registered resume handler with service');
            }
            else {
                onResumeCb();
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: resume ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const stopService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"stop",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Player: Registered stop handler with service');
            }
            else {
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
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: stop ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const seekService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"seek",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Player: Registered seek handler with service');
            }
            else {
                onSeekCb(null, message.value);
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: seek ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const setVolumeService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"setvolume",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Player: Registered setvolume handler with service');
            }
            else {
                onSetVolumeCb(null, message.value);
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: setvolume ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    const setSpeedService = window.webOS.service.request(`luna://${serviceId}/`, {
        method:"setspeed",
        parameters: {},
        onSuccess: (message: any) => {
            if (message.value.subscribed === true) {
                console.log('Player: Registered setspeed handler with service');
            }
            else {
                onSetSpeedCb(null, message.value);
            }
        },
        onFailure: (message: any) => {
            console.error(`Player: setspeed ${JSON.stringify(message)}`);
        },
        subscribe: true,
        resubscribe: true
    });

    window.targetAPI = {
        sendPlaybackError: (error: PlaybackErrorMessage) => {
            window.webOS.service.request(`luna://${serviceId}/`, {
                method: 'send_playback_error',
                parameters: { error },
                onSuccess: () => {},
                onFailure: (message: any) => {
                    console.error(`Player: send_playback_error ${JSON.stringify(message)}`);
                },
            });
        },

        sendPlaybackUpdate: (update: PlaybackUpdateMessage) => {
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
        },
        sendVolumeUpdate: (update: VolumeUpdateMessage) => {
            window.webOS.service.request(`luna://${serviceId}/`, {
                method: 'send_volume_update',
                parameters: { update },
                onSuccess: () => {},
                onFailure: (message: any) => {
                    console.error(`Player: send_volume_update ${JSON.stringify(message)}`);
                },
            });
        },
        onPlay: (callback: any) => { onPlayCb = callback; },
        onPause: (callback: any) => { onPauseCb = callback; },
        onResume: (callback: any) => { onResumeCb = callback; },
        onSeek: (callback: any) => { onSeekCb = callback; },
        onSetVolume: (callback: any) => { onSetVolumeCb = callback; },
        onSetSpeed: (callback: any) => { onSetSpeedCb = callback; }
    };

    window.webOSAPI = {
        pendingPlay: null
    };

} else {
    // @ts-ignore
    console.log(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}
