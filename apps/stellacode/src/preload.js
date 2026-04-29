const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('stellacode', {
  loadSettings: () => ipcRenderer.invoke('settings:load'),
  saveSettings: (settings) => ipcRenderer.invoke('settings:save', settings),
  request: (payload) => ipcRenderer.invoke('server:request', payload),
  connectionInfo: (serverId) => ipcRenderer.invoke('server:connectionInfo', serverId),
  stopTunnel: (serverId) => ipcRenderer.invoke('server:stopTunnel', serverId),
  uploadWorkspace: (payload) => ipcRenderer.invoke('workspace:upload', payload),
  downloadWorkspace: (payload) => ipcRenderer.invoke('workspace:download', payload),
  platform: process.platform,

  // Auto-updater
  updater: {
    check: () => ipcRenderer.invoke('updater:check'),
    download: () => ipcRenderer.invoke('updater:download'),
    install: () => ipcRenderer.invoke('updater:install'),
    onUpdateAvailable: (callback) => {
      ipcRenderer.on('updater:update-available', (_event, info) => callback(info));
    },
    onUpdateNotAvailable: (callback) => {
      ipcRenderer.on('updater:update-not-available', () => callback());
    },
    onDownloadProgress: (callback) => {
      ipcRenderer.on('updater:download-progress', (_event, progress) => callback(progress));
    },
    onUpdateDownloaded: (callback) => {
      ipcRenderer.on('updater:update-downloaded', (_event, info) => callback(info));
    }
  }
});
