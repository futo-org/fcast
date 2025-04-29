/* eslint-disable @typescript-eslint/ban-ts-comment */
/* eslint-disable @typescript-eslint/no-explicit-any */
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage, Opcode } from 'common/Packets';
export {};

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
        getSessions: () => electronAPI.ipcRenderer.invoke('get-sessions'),
        sendSessionMessage: (opcode: Opcode, message: any) => electronAPI.ipcRenderer.send('send-session-message', { opcode: opcode, message: message }),
        disconnectDevice: (session: string) => electronAPI.ipcRenderer.send('disconnect-device', session),
        onConnect: (callback: any) => electronAPI.ipcRenderer.on('connect', callback),
        onDisconnect: (callback: any) => electronAPI.ipcRenderer.on('disconnect', callback),
        onPing: (callback: any) => electronAPI.ipcRenderer.on('ping', callback),
        onPong: (callback: any) => electronAPI.ipcRenderer.on('pong', callback),
    });

// @ts-ignore
} else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
    preloadData = {
        sendPlaybackErrorCb: () => { console.error('Player: Callback "send_playback_error" not set'); },
        sendPlaybackUpdateCb: () => { console.error('Player: Callback "send_playback_update" not set'); },
        sendVolumeUpdateCb: () => { console.error('Player: Callback "send_volume_update" not set'); },
        // onPlayCb: () => { console.error('Player: Callback "play" not set'); },
        onPlayCb: undefined,
        onPauseCb: () => { console.error('Player: Callback "pause" not set'); },
        onResumeCb: () => { console.error('Player: Callback "resume" not set'); },
        onSeekCb: () => { console.error('Player: Callback "onseek" not set'); },
        onSetVolumeCb: () => { console.error('Player: Callback "setvolume" not set'); },
        onSetSpeedCb: () => { console.error('Player: Callback "setspeed" not set'); },
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
        onSetSpeed: (callback: any) => { preloadData.onSetSpeedCb = callback; }
    };
} else {
    // @ts-ignore
    console.log(`Attempting to run FCast player on unsupported target: ${TARGET}`);
}

export {
    preloadData
};
