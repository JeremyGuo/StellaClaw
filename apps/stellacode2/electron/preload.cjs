const { contextBridge, ipcRenderer } = require('electron');

function chromeMetrics() {
  if (process.platform === 'darwin') {
    return {
      platform: 'darwin',
      leftSafeArea: 86,
      leftToolbarOffset: 92,
      titleLeftOffset: 190,
      rightToolbarOffset: 12,
      titleRightOffset: 176,
      titleRightOffsetWithUpdate: 252
    };
  }
  if (process.platform === 'win32') {
    return {
      platform: 'win32',
      leftSafeArea: 0,
      leftToolbarOffset: 12,
      titleLeftOffset: 154,
      rightToolbarOffset: 150,
      titleRightOffset: 314,
      titleRightOffsetWithUpdate: 390
    };
  }
  return {
    platform: process.platform,
    leftSafeArea: 0,
    leftToolbarOffset: 12,
    titleLeftOffset: 154,
    rightToolbarOffset: 150,
    titleRightOffset: 314,
    titleRightOffsetWithUpdate: 390
  };
}

contextBridge.exposeInMainWorld('stellacode2', {
  chromeMetrics,
  appVersion: () => ipcRenderer.invoke('app:version'),
  loadSettings: () => ipcRenderer.invoke('settings:load'),
  saveSettings: (settings) => ipcRenderer.invoke('settings:save', settings),
  connectionInfo: (serverId) => ipcRenderer.invoke('server:connectionInfo', serverId),
  request: (payload) => ipcRenderer.invoke('server:request', payload),
  notify: (payload) => ipcRenderer.invoke('app:notify', payload),
  gzip: (payload) => ipcRenderer.invoke('binary:gzip', payload),
  uploadWorkspace: (payload) => ipcRenderer.invoke('workspace:upload', payload),
  downloadWorkspace: (payload) => ipcRenderer.invoke('workspace:download', payload),
  updater: {
    status: () => ipcRenderer.invoke('updater:status'),
    check: () => ipcRenderer.invoke('updater:check'),
    install: () => ipcRenderer.invoke('updater:install'),
    onStatus: (callback) => {
      const listener = (_event, status) => callback(status);
      ipcRenderer.on('updater:status', listener);
      return () => ipcRenderer.removeListener('updater:status', listener);
    }
  }
});
