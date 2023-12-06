const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('electronAPI', {
    onDeviceInfo: (callback) => ipcRenderer.on("device-info", callback)
});