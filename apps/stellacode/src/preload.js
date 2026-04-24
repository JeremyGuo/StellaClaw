const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('stellacode', {
  loadSettings: () => ipcRenderer.invoke('settings:load'),
  saveSettings: (settings) => ipcRenderer.invoke('settings:save', settings),
  request: (payload) => ipcRenderer.invoke('server:request', payload),
  stopTunnel: (serverId) => ipcRenderer.invoke('server:stopTunnel', serverId),
  platform: process.platform
});
