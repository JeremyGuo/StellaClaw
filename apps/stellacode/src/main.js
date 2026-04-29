const { app, BrowserWindow, ipcMain, dialog } = require('electron');
const { autoUpdater } = require('electron-updater');
const childProcess = require('node:child_process');
const fs = require('node:fs/promises');
const net = require('node:net');
const path = require('node:path');

const SETTINGS_FILE = 'settings.json';
const SSH_READY_DELAY_MS = 900;
const SERVER_REQUEST_TIMEOUT_MS = 90_000;

let mainWindow;
const tunnels = new Map();

function defaultSettings() {
  return {
    activeServerId: 'local',
    servers: [
      {
        id: 'local',
        name: 'Local Stellaclaw',
        connectionMode: 'direct',
        baseUrl: 'http://127.0.0.1:3111',
        targetUrl: 'http://127.0.0.1:3111',
        sshHost: '',
        token: 'local-web-token'
      }
    ],
    conversationNames: {},
    hiddenConversations: {},
    invalidModelAliases: {}
  };
}

function settingsPath() {
  return path.join(app.getPath('userData'), SETTINGS_FILE);
}

function normalizeSettings(value) {
  const fallback = defaultSettings();
  const servers = Array.isArray(value?.servers) ? value.servers : fallback.servers;
  const normalizedServers = servers.map((server, index) => ({
    id: String(server.id || `server-${index + 1}`),
    name: String(server.name || server.id || `Server ${index + 1}`),
    connectionMode: server.connectionMode === 'ssh_proxy' ? 'ssh_proxy' : 'direct',
    baseUrl: String(server.baseUrl || 'http://127.0.0.1:3111'),
    targetUrl: String(server.targetUrl || server.baseUrl || 'http://127.0.0.1:3111'),
    sshHost: String(server.sshHost || ''),
    token: String(server.token || '')
  }));
  return {
    activeServerId:
      value?.activeServerId && normalizedServers.some((server) => server.id === value.activeServerId)
        ? value.activeServerId
        : normalizedServers[0]?.id || fallback.activeServerId,
    servers: normalizedServers.length ? normalizedServers : fallback.servers,
    conversationNames:
      value?.conversationNames && typeof value.conversationNames === 'object'
        ? value.conversationNames
        : {},
    hiddenConversations:
      value?.hiddenConversations && typeof value.hiddenConversations === 'object'
        ? value.hiddenConversations
        : {},
    invalidModelAliases:
      value?.invalidModelAliases && typeof value.invalidModelAliases === 'object'
        ? value.invalidModelAliases
        : {}
  };
}

async function readSettings() {
  try {
    const raw = await fs.readFile(settingsPath(), 'utf8');
    return normalizeSettings(JSON.parse(raw));
  } catch (error) {
    if (error.code !== 'ENOENT') {
      console.warn('failed to read settings:', error);
    }
    return defaultSettings();
  }
}

async function writeSettings(settings) {
  const normalized = normalizeSettings(settings);
  await fs.mkdir(path.dirname(settingsPath()), { recursive: true });
  await fs.writeFile(settingsPath(), `${JSON.stringify(normalized, null, 2)}\n`, 'utf8');
  return normalized;
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1440,
    height: 920,
    minWidth: 1040,
    minHeight: 720,
    title: 'Stellacode',
    titleBarStyle: 'hiddenInset',
    trafficLightPosition: { x: 18, y: 18 },
    backgroundColor: '#111315',
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false
    }
  });

  mainWindow.loadFile(path.join(__dirname, 'index.html'));
}

function normalizeBaseUrl(value) {
  const url = new URL(value);
  url.hash = '';
  url.search = '';
  return url.toString().replace(/\/$/, '');
}

function joinApiUrl(baseUrl, apiPath) {
  const base = `${normalizeBaseUrl(baseUrl)}/`;
  const relative = String(apiPath || '').replace(/^\/+/, '');
  return new URL(relative, base).toString();
}

function shouldRetryRequest(method, status) {
  if (String(method || 'GET').toUpperCase() !== 'GET') {
    return false;
  }
  return status === 408 || status === 425 || status === 429 || (status >= 500 && status <= 599);
}

function retryDelayMs(attempt) {
  return [250, 700, 1500][Math.min(attempt, 2)];
}

async function sleep(ms) {
  await new Promise((resolve) => setTimeout(resolve, ms));
}

async function findFreePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      server.close(() => resolve(address.port));
    });
  });
}

async function resolveServerBaseUrl(server) {
  if (server.connectionMode !== 'ssh_proxy') {
    return normalizeBaseUrl(server.baseUrl);
  }
  if (!server.sshHost.trim()) {
    throw new Error('SSH proxy server is missing sshHost.');
  }
  const target = new URL(server.targetUrl || server.baseUrl);
  const existing = tunnels.get(server.id);
  if (existing && !existing.process.killed) {
    return existing.baseUrl;
  }

  const port = await findFreePort();
  const targetPort = target.port || (target.protocol === 'https:' ? '443' : '80');
  const bind = `127.0.0.1:${port}:${target.hostname}:${targetPort}`;
  const args = [
    '-N',
    '-L',
    bind,
    '-o',
    'ExitOnForwardFailure=yes',
    '-o',
    'ServerAliveInterval=20',
    '-o',
    'ServerAliveCountMax=2',
    server.sshHost
  ];
  const process = childProcess.spawn('ssh', args, {
    stdio: 'ignore',
    detached: false
  });

  let earlyExit = false;
  process.once('exit', () => {
    earlyExit = true;
    tunnels.delete(server.id);
  });

  await new Promise((resolve) => setTimeout(resolve, SSH_READY_DELAY_MS));
  if (earlyExit) {
    throw new Error('SSH tunnel exited before it became ready.');
  }

  const basePath = target.pathname && target.pathname !== '/' ? target.pathname.replace(/\/$/, '') : '';
  const baseUrl = `${target.protocol}//127.0.0.1:${port}${basePath}`;
  tunnels.set(server.id, { process, baseUrl });
  return baseUrl;
}

async function requestServer(_event, payload) {
  const settings = await readSettings();
  const server = settings.servers.find((item) => item.id === payload.serverId);
  if (!server) {
    throw new Error(`Unknown server: ${payload.serverId}`);
  }
  const baseUrl = await resolveServerBaseUrl(server);
  const headers = {
    Accept: 'application/json',
    Authorization: `Bearer ${server.token}`
  };
  const options = {
    method: payload.method || 'GET',
    headers
  };
  if (payload.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    options.body = JSON.stringify(payload.body);
  }

  const url = joinApiUrl(baseUrl, payload.path);
  let response;
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), SERVER_REQUEST_TIMEOUT_MS);
    try {
      response = await fetch(url, { ...options, signal: controller.signal });
    } catch (error) {
      if (String(options.method || 'GET').toUpperCase() !== 'GET' || attempt === 2) {
        throw error;
      }
      await sleep(retryDelayMs(attempt));
      continue;
    } finally {
      clearTimeout(timeout);
    }
    if (!shouldRetryRequest(options.method, response.status) || attempt === 2) {
      break;
    }
    await sleep(retryDelayMs(attempt));
  }
  const text = await response.text();
  let data = null;
  if (text.trim()) {
    try {
      data = JSON.parse(text);
    } catch {
      data = { text };
    }
  }
  if (!response.ok) {
    const message = data?.error || data?.message || `${response.status} ${response.statusText}`;
    throw new Error(message);
  }
  return {
    status: response.status,
    data
  };
}

async function serverConnectionInfo(_event, serverId) {
  const settings = await readSettings();
  const server = settings.servers.find((item) => item.id === serverId);
  if (!server) {
    throw new Error(`Unknown server: ${serverId}`);
  }
  return {
    baseUrl: await resolveServerBaseUrl(server),
    token: server.token
  };
}

function stopTunnel(serverId) {
  const tunnel = tunnels.get(serverId);
  if (!tunnel) {
    return false;
  }
  tunnel.process.kill('SIGTERM');
  tunnels.delete(serverId);
  return true;
}

function stopAllTunnels() {
  for (const serverId of tunnels.keys()) {
    stopTunnel(serverId);
  }
}

async function uploadWorkspaceFile(_event, payload) {
  const settings = await readSettings();
  const server = settings.servers.find((item) => item.id === payload.serverId);
  if (!server) {
    throw new Error(`Unknown server: ${payload.serverId}`);
  }
  const baseUrl = await resolveServerBaseUrl(server);
  const url = joinApiUrl(
    baseUrl,
    `/api/conversations/${payload.conversationId}/workspace/upload?path=${encodeURIComponent(payload.path || '')}`
  );
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), SERVER_REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(url, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${server.token}`,
        'Content-Type': 'application/gzip'
      },
      body: Buffer.from(payload.data),
      signal: controller.signal
    });
    if (!response.ok) {
      const text = await response.text();
      let message = `${response.status} ${response.statusText}`;
      try {
        const json = JSON.parse(text);
        message = json.error || json.message || message;
      } catch {}
      throw new Error(message);
    }
    return await response.json();
  } finally {
    clearTimeout(timeout);
  }
}

async function downloadWorkspaceFile(_event, payload) {
  const settings = await readSettings();
  const server = settings.servers.find((item) => item.id === payload.serverId);
  if (!server) {
    throw new Error(`Unknown server: ${payload.serverId}`);
  }
  const baseUrl = await resolveServerBaseUrl(server);
  const url = joinApiUrl(
    baseUrl,
    `/api/conversations/${payload.conversationId}/workspace/download?path=${encodeURIComponent(payload.path)}`
  );
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), SERVER_REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(url, {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${server.token}`,
        Accept: 'application/gzip'
      },
      signal: controller.signal
    });
    if (!response.ok) {
      const text = await response.text();
      let message = `${response.status} ${response.statusText}`;
      try {
        const json = JSON.parse(text);
        message = json.error || json.message || message;
      } catch {}
      throw new Error(message);
    }
    const buffer = Buffer.from(await response.arrayBuffer());
    const fileName = payload.suggestedName || `${path.basename(payload.path) || 'workspace'}.tar.gz`;
    const win = BrowserWindow.getFocusedWindow();
    const result = await dialog.showSaveDialog(win, {
      defaultPath: fileName,
      filters: [{ name: 'tar.gz archive', extensions: ['tar.gz'] }]
    });
    if (result.canceled || !result.filePath) {
      return { saved: false };
    }
    await fs.writeFile(result.filePath, buffer);
    return { saved: true, filePath: result.filePath, size: buffer.length };
  } finally {
    clearTimeout(timeout);
  }
}

// ── Auto-updater ──────────────────────────────────────────────────────
const UPDATE_CHECK_INTERVAL_MS = 10 * 60 * 1000; // 10 minutes
let updateCheckTimer = null;

function setupAutoUpdater() {
  autoUpdater.autoDownload = false;
  autoUpdater.autoInstallOnAppQuit = true;

  autoUpdater.on('update-available', (info) => {
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send('updater:update-available', {
        version: info.version,
        releaseDate: info.releaseDate
      });
    }
  });

  autoUpdater.on('update-not-available', () => {
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send('updater:update-not-available');
    }
  });

  autoUpdater.on('download-progress', (progress) => {
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send('updater:download-progress', {
        percent: progress.percent,
        bytesPerSecond: progress.bytesPerSecond,
        transferred: progress.transferred,
        total: progress.total
      });
    }
  });

  autoUpdater.on('update-downloaded', (info) => {
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send('updater:update-downloaded', {
        version: info.version
      });
    }
  });

  autoUpdater.on('error', (error) => {
    console.warn('Auto-updater error:', error?.message || error);
  });

  // Check now, then every 10 minutes.
  autoUpdater.checkForUpdates().catch(() => {});
  updateCheckTimer = setInterval(() => {
    autoUpdater.checkForUpdates().catch(() => {});
  }, UPDATE_CHECK_INTERVAL_MS);
}

app.whenReady().then(() => {
  ipcMain.handle('settings:load', readSettings);
  ipcMain.handle('settings:save', (_event, settings) => writeSettings(settings));
  ipcMain.handle('server:request', requestServer);
  ipcMain.handle('server:connectionInfo', serverConnectionInfo);
  ipcMain.handle('server:stopTunnel', (_event, serverId) => stopTunnel(serverId));
  ipcMain.handle('workspace:upload', uploadWorkspaceFile);
  ipcMain.handle('workspace:download', downloadWorkspaceFile);

  // Updater IPC
  ipcMain.handle('updater:check', () => autoUpdater.checkForUpdates().catch(() => null));
  ipcMain.handle('updater:download', () => autoUpdater.downloadUpdate().catch(() => null));
  ipcMain.handle('updater:install', () => {
    autoUpdater.quitAndInstall(false, true);
  });

  createWindow();
  setupAutoUpdater();

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) {
      createWindow();
    }
  });
});

app.on('before-quit', stopAllTunnels);

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') {
    app.quit();
  }
});
