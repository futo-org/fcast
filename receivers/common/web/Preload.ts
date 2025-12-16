/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { toast, ToastIcon } from 'common/components/Toast';
import { Logger, LoggerType } from 'common/Logger';
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage, EventMessage, PlayMessage } from 'common/Packets';
const logger = new Logger('RendererWindow', LoggerType.FRONTEND);

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
      webOSDev: any;
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

    electronAPI.ipcRenderer.on('device-info', (_event: any, value: any) => {
        preloadData.deviceInfo = value;
    })
    electronAPI.ipcRenderer.on('event-subscribed-keys-update', (_event: any, value: { keyDown: Set<string>, keyUp: Set<string> }) => {
        preloadData.subscribedKeys.keyDown = value.keyDown;
        preloadData.subscribedKeys.keyUp = value.keyUp;
    })

    electronAPI.ipcRenderer.on('update-background', (_event: any, path: string, isVideo: boolean) => {
        const imageBackground = document.getElementById('image-background') as HTMLImageElement;
        const videoBackground = document.getElementById('video-player') as HTMLVideoElement;

        if (isVideo) {
            videoBackground.src = path;

            imageBackground.style.display = 'none';
            videoBackground.style.display = 'block';
        }
        else {
            imageBackground.src = path;

            imageBackground.style.display = 'block';
            videoBackground.style.display = 'none';
        }
    })

    electronAPI.ipcRenderer.on('toast', (_event: any, message: string, icon: ToastIcon = ToastIcon.INFO, duration: number = 5000) => {
        toast(message, icon, duration);
    })

    electronAPI.contextBridge.exposeInMainWorld('targetAPI', {
        // Main window
        onDeviceInfo: (callback: any) => electronAPI.ipcRenderer.on('device-info', callback),
        getDeviceInfo: () => preloadData.deviceInfo,

        // Common
        initializeSubscribedKeys: () => {
            electronAPI.ipcRenderer.invoke('get-subscribed-keys').then((value: { keyDown: Set<string>, keyUp: Set<string> }) => {
                preloadData.subscribedKeys.keyDown = value.keyDown;
                preloadData.subscribedKeys.keyUp = value.keyUp;
            });
        },
        getSessions: () => electronAPI.ipcRenderer.invoke('get-sessions'),
        getSubscribedKeys: () => preloadData.subscribedKeys,
        onConnect: (callback: any) => electronAPI.ipcRenderer.on('connect', callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on('disconnect', callback),
        sendEvent: (message: EventMessage) => electronAPI.ipcRenderer.send('send-event', message),
        logger: loggerInterface,

        // Player window
        sendPlaybackUpdate: (update: PlaybackUpdateMessage) => electronAPI.ipcRenderer.send('send-playback-update', update),
        sendVolumeUpdate: (update: VolumeUpdateMessage) => electronAPI.ipcRenderer.send('send-volume-update', update),
        sendPlaybackError: (error: PlaybackErrorMessage) => electronAPI.ipcRenderer.send('send-playback-error', error),
        onPlay: (callback: any) => electronAPI.ipcRenderer.on("play", callback),
        onPause: (callback: any) => electronAPI.ipcRenderer.on("pause", callback),
        onResume: (callback: any) => electronAPI.ipcRenderer.on("resume", callback),
        onSeek: (callback: any) => electronAPI.ipcRenderer.on("seek", callback),
        onSetVolume: (callback: any) => electronAPI.ipcRenderer.on("setvolume", callback),
        onSetSpeed: (callback: any) => electronAPI.ipcRenderer.on("setspeed", callback),
        onSetPlaylistItem: (callback: any) => electronAPI.ipcRenderer.on("setplaylistitem", callback),
        sendPlayRequest: (message: PlayMessage, playlistIndex: number) => electronAPI.ipcRenderer.send('play-request', message, playlistIndex),
        onPlayPlaylist: (callback: any) => electronAPI.ipcRenderer.on('play-playlist', callback),
    });

// @ts-ignore
} else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
    // Main window
    preloadData.onDeviceInfoCb = () => { logger.warn('RendererWindow: Callback not set while fetching device info'); };

    // Common
    preloadData.getSessionsCb = () => { logger.error('RendererWindow: Callback not set while calling getSessions'); };
    preloadData.initializeSubscribedKeysCb = () => { logger.error('RendererWindow: Callback not set while calling initializeSubscribedKeys'); };
    preloadData.onConnectCb = (_, value: any) => { logger.error('RendererWindow: Callback not set while calling onConnect'); };
    preloadData.onDisconnectCb = (_, value: any) => { logger.error('RendererWindow: Callback not set while calling onDisconnect'); };
    preloadData.sendEventCb = (message: EventMessage) => { logger.error('RendererWindow: Callback not set while calling onSendEventCb'); };

    // Player window
    preloadData.sendPlaybackUpdateCb = (update: PlaybackUpdateMessage) => { logger.error('RendererWindow: Callback "send_playback_update" not set'); };
    preloadData.sendVolumeUpdateCb = (update: VolumeUpdateMessage) => { logger.error('RendererWindow: Callback "send_volume_update" not set'); };
    preloadData.sendPlaybackErrorCb = (error: PlaybackErrorMessage) => { logger.error('RendererWindow: Callback "send_playback_error" not set'); };
    // preloadData.onPlayCb = () => { logger.error('RendererWindow: Callback "play" not set'); };
    preloadData.onPlayCb = undefined;
    preloadData.onPauseCb = () => { logger.error('RendererWindow: Callback "pause" not set'); };
    preloadData.onResumeCb = () => { logger.error('RendererWindow: Callback "resume" not set'); };
    preloadData.onSeekCb = () => { logger.error('RendererWindow: Callback "onseek" not set'); };
    preloadData.onSetVolumeCb = () => { logger.error('RendererWindow: Callback "setvolume" not set'); };
    preloadData.onSetSpeedCb = () => { logger.error('RendererWindow: Callback "setspeed" not set'); };
    preloadData.onSetPlaylistItemCb = () => { logger.error('RendererWindow: Callback "onSetPlaylistItem" not set'); };
    preloadData.sendPlayRequestCb = () => { logger.error('RendererWindow: Callback "sendPlayRequest" not set'); };
    preloadData.onPlayPlaylistCb = () => { logger.error('RendererWindow: Callback "onPlayPlaylist" not set'); };

    preloadData.onEventSubscribedKeysUpdate = (value: { keyDown: string[], keyUp: string[] }) => {
        preloadData.subscribedKeys.keyDown = new Set(value.keyDown);
        preloadData.subscribedKeys.keyUp = new Set(value.keyUp);
    };

    preloadData.onToastCb = (message: string, icon: ToastIcon = ToastIcon.INFO, duration: number = 5000) => {
        toast(message, icon, duration);
    };

    window.targetAPI = {
        // Main window
        onDeviceInfo: (callback: () => void) => preloadData.onDeviceInfoCb = callback,
        getDeviceInfo: () => preloadData.deviceInfo,

        // Common
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
        sendEvent: (message: EventMessage) =>  { preloadData.sendEventCb(message); },
        logger: loggerInterface,

        // Player window
        sendPlaybackUpdate: (update: PlaybackUpdateMessage) => { preloadData.sendPlaybackUpdateCb(update); },
        sendVolumeUpdate: (update: VolumeUpdateMessage) => { preloadData.sendVolumeUpdateCb(update); },
        sendPlaybackError: (error: PlaybackErrorMessage) => { preloadData.sendPlaybackErrorCb(error); },
        onPlay: (callback: any) => { preloadData.onPlayCb = callback; },
        onPause: (callback: any) => { preloadData.onPauseCb = callback; },
        onResume: (callback: any) => { preloadData.onResumeCb = callback; },
        onSeek: (callback: any) => { preloadData.onSeekCb = callback; },
        onSetVolume: (callback: any) => { preloadData.onSetVolumeCb = callback; },
        onSetSpeed: (callback: any) => { preloadData.onSetSpeedCb = callback; },
        onSetPlaylistItem: (callback: any) => { preloadData.onSetPlaylistItemCb = callback; },
        sendPlayRequest: (message: PlayMessage, playlistIndex: number) => { preloadData.sendPlayRequestCb(message, playlistIndex); },
        onPlayPlaylist: (callback: any) => { preloadData.onPlayPlaylistCb = callback; },
    };
} else {
    // @ts-ignore
    logger.warn(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
