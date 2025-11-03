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
        initializeSubscribedKeys: () => {
            electronAPI.ipcRenderer.invoke('get-subscribed-keys').then((value: { keyDown: Set<string>, keyUp: Set<string> }) => {
                preloadData.subscribedKeys.keyDown = value.keyDown;
                preloadData.subscribedKeys.keyUp = value.keyUp;
            });
        },
        getSubscribedKeys: () => preloadData.subscribedKeys,
        onConnect: (callback: any) => electronAPI.ipcRenderer.on('connect', callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on('disconnect', callback),
        onPlayPlaylist: (callback: any) => electronAPI.ipcRenderer.on('play-playlist', callback),
        logger: loggerInterface,
    });

// @ts-ignore
} else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
    preloadData.sendPlaybackUpdateCb = (update: PlaybackUpdateMessage) => { logger.error('Player: Callback "send_playback_update" not set'); };
    preloadData.sendVolumeUpdateCb = (update: VolumeUpdateMessage) => { logger.error('Player: Callback "send_volume_update" not set'); };
    preloadData.sendPlaybackErrorCb = (error: PlaybackErrorMessage) => { logger.error('Player: Callback "send_playback_error" not set'); };
    preloadData.sendEventCb = (message: EventMessage) => { logger.error('Player: Callback "onSendEventCb" not set'); };
    // preloadData.onPlayCb = () => { logger.error('Player: Callback "play" not set'); };
    preloadData.onPlayCb = undefined;
    preloadData.onPauseCb = () => { logger.error('Player: Callback "pause" not set'); };
    preloadData.onResumeCb = () => { logger.error('Player: Callback "resume" not set'); };
    preloadData.onSeekCb = () => { logger.error('Player: Callback "onseek" not set'); };
    preloadData.onSetVolumeCb = () => { logger.error('Player: Callback "setvolume" not set'); };
    preloadData.onSetSpeedCb = () => { logger.error('Player: Callback "setspeed" not set'); };
    preloadData.onSetPlaylistItemCb = () => { logger.error('Player: Callback "onSetPlaylistItem" not set'); };

    preloadData.sendPlayRequestCb = () => { logger.error('Player: Callback "sendPlayRequest" not set'); };
    preloadData.getSessionsCb = () => { logger.error('Player: Callback "getSessions" not set'); };
    preloadData.initializeSubscribedKeysCb = () => { logger.error('Player: Callback "initializeSubscribedKeys" not set'); };
    preloadData.onConnectCb = () => { logger.warn('Player: Callback "onConnect" not set'); };
    preloadData.onDisconnectCb = () => { logger.warn('Player: Callback "onDisconnect" not set'); };
    preloadData.onPlayPlaylistCb = () => { logger.error('Player: Callback "onPlayPlaylist" not set'); };

    preloadData.onEventSubscribedKeysUpdate = (value: { keyDown: string[], keyUp: string[] }) => {
        preloadData.subscribedKeys.keyDown = new Set(value.keyDown);
        preloadData.subscribedKeys.keyUp = new Set(value.keyUp);
    };

    preloadData.onToastCb = (message: string, icon: ToastIcon = ToastIcon.INFO, duration: number = 5000) => {
        toast(message, icon, duration);
    };

    window.targetAPI = {
        sendPlaybackUpdate: (update: PlaybackUpdateMessage) => { preloadData.sendPlaybackUpdateCb(update); },
        sendVolumeUpdate: (update: VolumeUpdateMessage) => { preloadData.sendVolumeUpdateCb(update); },
        sendPlaybackError: (error: PlaybackErrorMessage) => { preloadData.sendPlaybackErrorCb(error); },
        sendEvent: (message: EventMessage) =>  { preloadData.sendEventCb(message); },
        onPlay: (callback: any) => { preloadData.onPlayCb = callback; },
        onPause: (callback: any) => { preloadData.onPauseCb = callback; },
        onResume: (callback: any) => { preloadData.onResumeCb = callback; },
        onSeek: (callback: any) => { preloadData.onSeekCb = callback; },
        onSetVolume: (callback: any) => { preloadData.onSetVolumeCb = callback; },
        onSetSpeed: (callback: any) => { preloadData.onSetSpeedCb = callback; },
        onSetPlaylistItem: (callback: any) => { preloadData.onSetPlaylistItemCb = callback; },

        sendPlayRequest: (message: PlayMessage, playlistIndex: number) => { preloadData.sendPlayRequestCb(message, playlistIndex); },
        getSessions: (callback?: () => Promise<[any]>) => {
            if (callback) {
                preloadData.getSessionsCb = callback;
            }
            else {
                return preloadData.getSessionsCb();
            }
        },
        initializeSubscribedKeys: (callback?: () => Promise<{ keyDown: string[], keyUp: string[] }>) => {
            if (callback) {
                preloadData.initializeSubscribedKeysCb = callback;
            }
            else {
                preloadData.initializeSubscribedKeysCb().then((value: { keyDown: Set<string>, keyUp: Set<string> }) => {
                    preloadData.subscribedKeys.keyDown = new Set(value.keyDown);
                    preloadData.subscribedKeys.keyUp = new Set(value.keyUp);
                });
            }
        },
        getSubscribedKeys: () => preloadData.subscribedKeys,
        onConnect: (callback: any) => { preloadData.onConnectCb = callback; },
        onDisconnect: (callback: any) => { preloadData.onDisconnectCb = callback; },
        onPlayPlaylist: (callback: any) => { preloadData.onPlayPlaylistCb = callback; },
        logger: loggerInterface,
    };
} else {
    // @ts-ignore
    logger.warn(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
