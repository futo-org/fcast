const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('electronAPI', {
    toggleFullScreen: () => ipcRenderer.send('toggle-full-screen'),
    exitFullScreen: () => ipcRenderer.send('exit-full-screen'),
    sendPlaybackUpdate: (update) => ipcRenderer.send('send-playback-update', update),
    sendVolumeUpdate: (update) => ipcRenderer.send('send-volume-update', update),
    onPlay: (callback) => ipcRenderer.on("play", callback),
    onPause: (callback) => ipcRenderer.on("pause", callback),
    onResume: (callback) => ipcRenderer.on("resume", callback),
    onSeek: (callback) => ipcRenderer.on("seek", callback),
    onSetVolume: (callback) => ipcRenderer.on("setvolume", callback)
});