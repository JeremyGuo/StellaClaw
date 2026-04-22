const { contextBridge } = require('electron');

contextBridge.exposeInMainWorld('partxDesktop', {
  isElectron: true,
});
