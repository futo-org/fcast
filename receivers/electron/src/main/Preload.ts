import { contextBridge, ipcRenderer } from 'electron';

let deviceInfo;
ipcRenderer.on("device-info", (_event, value) => {
    deviceInfo = value;
})

contextBridge.exposeInMainWorld('electronAPI', {
    onDeviceInfo: (callback) => ipcRenderer.on("device-info", callback),
    getDeviceInfo: () => deviceInfo,
});
