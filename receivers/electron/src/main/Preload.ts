/* eslint-disable @typescript-eslint/no-explicit-any */
import { contextBridge, ipcRenderer } from 'electron';
import 'common/main/Preload';
import { toast } from 'common/components/Toast';

ipcRenderer.on("toast", (_event, value: any) => {
    toast(value.message, value.icon, value.duration);
});

contextBridge.exposeInMainWorld('electronAPI', {
    updaterProgress: () => ipcRenderer.invoke('updater-progress'),
    onUpdateAvailable: (callback: any) => ipcRenderer.on("update-available", callback),
    sendDownloadRequest: () => ipcRenderer.send('send-download-request'),
    onDownloadComplete: (callback: any) => ipcRenderer.on("download-complete", callback),
    onDownloadFailed: (callback: any) => ipcRenderer.on("download-failed", callback),
    sendRestartRequest: () => ipcRenderer.send('send-restart-request'),
});
