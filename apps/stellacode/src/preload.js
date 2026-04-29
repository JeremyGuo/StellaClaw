const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('stellacode', {
  loadSettings: () => ipcRenderer.invoke('settings:load'),
  saveSettings: (settings) => ipcRenderer.invoke('settings:save', settings),
  request: (payload) => ipcRenderer.invoke('server:request', payload),
  connectionInfo: (serverId) => ipcRenderer.invoke('server:connectionInfo', serverId),
  stopTunnel: (serverId) => ipcRenderer.invoke('server:stopTunnel', serverId),
  uploadWorkspace: (payload) => ipcRenderer.invoke('workspace:upload', payload),
  downloadWorkspace: (payload) => ipcRenderer.invoke('workspace:download', payload),
  platform: process.platform
});
