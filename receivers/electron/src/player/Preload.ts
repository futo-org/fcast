/* eslint-disable  @typescript-eslint/no-explicit-any */
import { contextBridge, ipcRenderer } from 'electron';
import { PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from '../Packets';

declare global {
    interface Window {
      electronAPI: any;
    }
}

contextBridge.exposeInMainWorld('electronAPI', {
    isFullScreen: () => ipcRenderer.invoke('is-full-screen'),
    toggleFullScreen: () => ipcRenderer.send('toggle-full-screen'),
    exitFullScreen: () => ipcRenderer.send('exit-full-screen'),
    sendPlaybackError: (error: PlaybackErrorMessage) => ipcRenderer.send('send-playback-error', error),
    sendPlaybackUpdate: (update: PlaybackUpdateMessage) => ipcRenderer.send('send-playback-update', update),
    sendVolumeUpdate: (update: VolumeUpdateMessage) => ipcRenderer.send('send-volume-update', update),
    onPlay: (callback: any) => ipcRenderer.on("play", callback),
    onPause: (callback: any) => ipcRenderer.on("pause", callback),
    onResume: (callback: any) => ipcRenderer.on("resume", callback),
    onSeek: (callback: any) => ipcRenderer.on("seek", callback),
    onSetVolume: (callback: any) => ipcRenderer.on("setvolume", callback),
    onSetSpeed: (callback: any) => ipcRenderer.on("setspeed", callback)
});
