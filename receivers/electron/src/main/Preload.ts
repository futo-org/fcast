import { contextBridge, ipcRenderer } from 'electron';

let deviceInfo;
ipcRenderer.on("device-info", (_event, value) => {
    deviceInfo = value;
})

contextBridge.exposeInMainWorld('electronAPI', {
    updaterProgress: () => ipcRenderer.invoke('updater-progress'),
    onDeviceInfo: (callback) => ipcRenderer.on("device-info", callback),
    onUpdateAvailable: (callback) => ipcRenderer.on("update-available", callback),
    sendDownloadRequest: () => ipcRenderer.send('send-download-request'),
    onDownloadComplete: (callback) => ipcRenderer.on("download-complete", callback),
    onDownloadFailed: (callback) => ipcRenderer.on("download-failed", callback),
    sendRestartRequest: () => ipcRenderer.send('send-restart-request'),
    getDeviceInfo: () => deviceInfo,
});
