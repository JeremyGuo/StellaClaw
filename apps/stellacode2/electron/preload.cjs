const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('stellacode2', {
  loadSettings: () => ipcRenderer.invoke('settings:load'),
  saveSettings: (settings) => ipcRenderer.invoke('settings:save', settings),
  connectionInfo: (serverId) => ipcRenderer.invoke('server:connectionInfo', serverId),
  request: (payload) => ipcRenderer.invoke('server:request', payload),
  gzip: (payload) => ipcRenderer.invoke('binary:gzip', payload),
  uploadWorkspace: (payload) => ipcRenderer.invoke('workspace:upload', payload),
  downloadWorkspace: (payload) => ipcRenderer.invoke('workspace:download', payload)
});
