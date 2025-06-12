/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage, EventMessage, PlayMessage } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';
import { toast, ToastIcon } from 'common/components/Toast';
const logger = new Logger('PlayerWindow', LoggerType.FRONTEND);

// Cannot directly pass the object to the renderer for some reason...
const loggerInterface = {
    trace: (message?: any, ...optionalParams: any[]) => { logger.trace(message, ...optionalParams); },
    debug: (message?: any, ...optionalParams: any[]) => { logger.debug(message, ...optionalParams); },
    info: (message?: any, ...optionalParams: any[]) => { logger.info(message, ...optionalParams); },
    warn: (message?: any, ...optionalParams: any[]) => { logger.warn(message, ...optionalParams); },
    error: (message?: any, ...optionalParams: any[]) => { logger.error(message, ...optionalParams); },
    fatal: (message?: any, ...optionalParams: any[]) => { logger.fatal(message, ...optionalParams); },
};

declare global {
    interface Window {
      electronAPI: any;
      tizenOSAPI: any;
      webOSAPI: any;
      webOS: any;
      targetAPI: any;
    }
}

let preloadData: Record<string, any> = {};
preloadData.subscribedKeys = {
    keyDown: new Set<string>(),
    keyUp: new Set<string>(),
};

// @ts-ignore
if (TARGET === 'electron') {
    // @ts-ignore
    const electronAPI = __non_webpack_require__('electron');

    electronAPI.ipcRenderer.on('event-subscribed-keys-update', (_event, value: { keyDown: Set<string>, keyUp: Set<string> }) => {
        preloadData.subscribedKeys.keyDown = value.keyDown;
        preloadData.subscribedKeys.keyUp = value.keyUp;
    })

    electronAPI.ipcRenderer.on('toast', (_event, message: string, icon: ToastIcon = ToastIcon.INFO, duration: number = 5000) => {
        toast(message, icon, duration);
    })

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        sendPlaybackUpdate: (update: PlaybackUpdateMessage) => electronAPI.ipcRenderer.send('send-playback-update', update),
        sendVolumeUpdate: (update: VolumeUpdateMessage) => electronAPI.ipcRenderer.send('send-volume-update', update),
        sendPlaybackError: (error: PlaybackErrorMessage) => electronAPI.ipcRenderer.send('send-playback-error', error),
        sendEvent: (message: EventMessage) => electronAPI.ipcRenderer.send('send-event', message),
        onPlay: (callback: any) => electronAPI.ipcRenderer.on("play", callback),
        onPause: (callback: any) => electronAPI.ipcRenderer.on("pause", callback),
        onResume: (callback: any) => electronAPI.ipcRenderer.on("resume", callback),
        onSeek: (callback: any) => electronAPI.ipcRenderer.on("seek", callback),
        onSetVolume: (callback: any) => electronAPI.ipcRenderer.on("setvolume", callback),
        onSetSpeed: (callback: any) => electronAPI.ipcRenderer.on("setspeed", callback),
        onSetPlaylistItem: (callback: any) => electronAPI.ipcRenderer.on("setplaylistitem", callback),

        sendPlayRequest: (message: PlayMessage, playlistIndex: number) => electronAPI.ipcRenderer.send('play-request', message, playlistIndex),
        getSessions: () => electronAPI.ipcRenderer.invoke('get-sessions'),
        getSubscribedKeys: () => preloadData.subscribedKeys,
        onConnect: (callback: any) => electronAPI.ipcRenderer.on('connect', callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on('disconnect', callback),
        onPlayPlaylist: (callback: any) => electronAPI.ipcRenderer.on('play-playlist', callback),
        logger: loggerInterface,
    });

// @ts-ignore
} else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
    preloadData = {
        sendPlaybackErrorCb: () => { logger.error('Player: Callback "send_playback_error" not set'); },
        sendPlaybackUpdateCb: () => { logger.error('Player: Callback "send_playback_update" not set'); },
        sendVolumeUpdateCb: () => { logger.error('Player: Callback "send_volume_update" not set'); },
        // onPlayCb: () => { logger.error('Player: Callback "play" not set'); },
        onPlayCb: undefined,
        onPauseCb: () => { logger.error('Player: Callback "pause" not set'); },
        onResumeCb: () => { logger.error('Player: Callback "resume" not set'); },
        onSeekCb: () => { logger.error('Player: Callback "onseek" not set'); },
        onSetVolumeCb: () => { logger.error('Player: Callback "setvolume" not set'); },
        onSetSpeedCb: () => { logger.error('Player: Callback "setspeed" not set'); },
        getSessionsCb: () => { logger.error('Player: Callback "getSessions" not set'); },
        onConnectCb: () => { logger.error('Player: Callback "onConnect" not set'); },
        onDisconnectCb: () => { logger.error('Player: Callback "onDisconnect" not set'); },
    };

    window.targetAPI = {
        sendPlaybackError: (error: PlaybackErrorMessage) => { preloadData.sendPlaybackErrorCb(error); },
        sendPlaybackUpdate: (update: PlaybackUpdateMessage) => { preloadData.sendPlaybackUpdateCb(update); },
        sendVolumeUpdate: (update: VolumeUpdateMessage) => { preloadData.sendVolumeUpdateCb(update); },
        onPlay: (callback: any) => { preloadData.onPlayCb = callback; },
        onPause: (callback: any) => { preloadData.onPauseCb = callback; },
        onResume: (callback: any) => { preloadData.onResumeCb = callback; },
        onSeek: (callback: any) => { preloadData.onSeekCb = callback; },
        onSetVolume: (callback: any) => { preloadData.onSetVolumeCb = callback; },
        onSetSpeed: (callback: any) => { preloadData.onSetSpeedCb = callback; },
        getSessions: (callback?: () => Promise<[any]>) => {
            if (callback) {
                preloadData.getSessionsCb = callback;
            }
            else {
                return preloadData.getSessionsCb();
            }
        },
        onConnect: (callback: any) => { preloadData.onConnectCb = callback; },
        onDisconnect: (callback: any) => { preloadData.onDisconnectCb = callback; },
        logger: loggerInterface,
    };
} else {
    // @ts-ignore
    logger.warn(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
