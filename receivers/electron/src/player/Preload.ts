import { contextBridge, ipcRenderer } from 'electron';
import 'common/player/Preload';

contextBridge.exposeInMainWorld('electronAPI', {
    isFullScreen: () => ipcRenderer.invoke('is-full-screen'),
    toggleFullScreen: () => ipcRenderer.send('toggle-full-screen'),
    exitFullScreen: () => ipcRenderer.send('exit-full-screen'),
});
