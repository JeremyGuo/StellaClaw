const { app, BrowserWindow, dialog, ipcMain, Notification } = require('electron');
const { autoUpdater } = require('electron-updater');
const childProcess = require('node:child_process');
const fs = require('node:fs/promises');
const net = require('node:net');
const path = require('node:path');
const zlib = require('node:zlib');

const SETTINGS_FILE = 'settings.json';
const SSH_READY_TIMEOUT_MS = 10_000;
const SERVER_REQUEST_TIMEOUT_MS = 90_000;
const UPDATE_CHECK_INTERVAL_MS = 10 * 60 * 1000;

let mainWindow;
let updateCheckTimer = null;
let updaterState = { state: app.isPackaged ? 'idle' : 'disabled' };
const tunnels = new Map();

function appIconPath() {
  return path.join(__dirname, '..', 'build', 'icon.png');
}

function defaultSettings() {
  return {
    activeServerId: 'local',
    sidebarMode: 'expanded',
    themeMode: 'system',
    layout: {
      sidebar: 286,
      inspector: 340,
      file: 360,
      preview: 480,
      terminal: 240,
      terminalList: 210
    },
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
    conversationUi: {},
    conversationRead: {}
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
  const layout = value?.layout && typeof value.layout === 'object' ? value.layout : {};
  return {
    activeServerId:
      value?.activeServerId && normalizedServers.some((server) => server.id === value.activeServerId)
        ? value.activeServerId
        : normalizedServers[0]?.id || fallback.activeServerId,
    sidebarMode: value?.sidebarMode === 'collapsed' ? 'collapsed' : 'expanded',
    themeMode: ['system', 'light', 'dark'].includes(value?.themeMode) ? value.themeMode : fallback.themeMode,
    layout: {
      sidebar: Number(layout.sidebar) || fallback.layout.sidebar,
      inspector: Number(layout.inspector) || fallback.layout.inspector,
      file: Number(layout.file) || fallback.layout.file,
      preview: Number(layout.preview) || fallback.layout.preview,
      terminal: Number(layout.terminal) || fallback.layout.terminal,
      terminalList: Number(layout.terminalList) || fallback.layout.terminalList
    },
    servers: normalizedServers.length ? normalizedServers : fallback.servers,
    conversationNames:
      value?.conversationNames && typeof value.conversationNames === 'object'
        ? value.conversationNames
        : {},
    hiddenConversations:
      value?.hiddenConversations && typeof value.hiddenConversations === 'object'
        ? value.hiddenConversations
        : {},
    conversationUi:
      value?.conversationUi && typeof value.conversationUi === 'object'
        ? value.conversationUi
        : {},
    conversationRead:
      value?.conversationRead && typeof value.conversationRead === 'object'
        ? value.conversationRead
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
  stopRemovedOrChangedTunnels(normalized.servers);
  await fs.mkdir(path.dirname(settingsPath()), { recursive: true });
  await fs.writeFile(settingsPath(), `${JSON.stringify(normalized, null, 2)}\n`, 'utf8');
  return normalized;
}

function normalizeBaseUrl(value) {
  return String(value || '').replace(/\/$/, '');
}

function joinApiUrl(baseUrl, requestPath) {
  const cleanBase = normalizeBaseUrl(baseUrl);
  const cleanPath = String(requestPath || '').startsWith('/') ? requestPath : `/${requestPath}`;
  return `${cleanBase}${cleanPath}`;
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

async function waitForLocalPort(port, getExitDetails, stderrLines) {
  const deadline = Date.now() + SSH_READY_TIMEOUT_MS;
  while (Date.now() < deadline) {
    const exitDetails = getExitDetails();
    if (exitDetails) {
      const suffix = stderrLines.length ? `: ${stderrLines.join('').trim()}` : '';
      throw new Error(`SSH tunnel exited early (${exitDetails.code ?? exitDetails.signal})${suffix}`);
    }
    const ready = await new Promise((resolve) => {
      const socket = net.createConnection({ host: '127.0.0.1', port });
      socket.once('connect', () => {
        socket.destroy();
        resolve(true);
      });
      socket.once('error', () => resolve(false));
      socket.setTimeout(200, () => {
        socket.destroy();
        resolve(false);
      });
    });
    if (ready) return;
    await sleep(50);
  }
  throw new Error('Timed out waiting for SSH tunnel local port.');
}

function tunnelSignature(server) {
  return `${server.sshHost.trim()}|${server.targetUrl || server.baseUrl}`;
}

function stopTunnel(serverId) {
  const existing = tunnels.get(serverId);
  if (!existing) return;
  tunnels.delete(serverId);
  if (!existing.process.killed) {
    existing.process.kill('SIGTERM');
  }
}

function stopRemovedOrChangedTunnels(servers) {
  const active = new Map((servers || []).map((server) => [server.id, server]));
  for (const [serverId, tunnel] of tunnels.entries()) {
    const server = active.get(serverId);
    if (!server || server.connectionMode !== 'ssh_proxy' || tunnel.signature !== tunnelSignature(server)) {
      stopTunnel(serverId);
    }
  }
}

async function resolveServerBaseUrl(server) {
  if (server.connectionMode !== 'ssh_proxy') {
    return normalizeBaseUrl(server.baseUrl);
  }
  const sshHost = server.sshHost.trim();
  if (!sshHost) {
    throw new Error('SSH proxy server is missing SSH Host or alias.');
  }
  const target = new URL(server.targetUrl || server.baseUrl);
  const signature = tunnelSignature(server);
  const existing = tunnels.get(server.id);
  if (existing && existing.signature === signature && !existing.process.killed) {
    return existing.baseUrl;
  }
  if (existing) {
    stopTunnel(server.id);
  }

  const port = await findFreePort();
  const targetPort = target.port || (target.protocol === 'https:' ? '443' : '80');
  const bind = `127.0.0.1:${port}:${target.hostname}:${targetPort}`;
  const process = childProcess.spawn('ssh', [
    '-N',
    '-T',
    '-L',
    bind,
    '-o',
    'ExitOnForwardFailure=no',
    '-o',
    'ServerAliveInterval=20',
    '-o',
    'ServerAliveCountMax=2',
    sshHost
  ], {
    stdio: ['ignore', 'ignore', 'pipe'],
    detached: false
  });

  const stderrLines = [];
  let exitDetails = null;
  process.stderr?.setEncoding('utf8');
  process.stderr?.on('data', (chunk) => {
    stderrLines.push(chunk);
    if (stderrLines.length > 8) stderrLines.shift();
  });
  process.once('exit', (code, signal) => {
    exitDetails = { code, signal };
    const current = tunnels.get(server.id);
    if (current?.process === process) {
      tunnels.delete(server.id);
    }
  });
  try {
    await waitForLocalPort(port, () => exitDetails, stderrLines);
  } catch (error) {
    if (!process.killed) process.kill('SIGTERM');
    throw new Error(`Failed to open SSH tunnel through ${sshHost}: ${error.message}`);
  }

  const basePath = target.pathname && target.pathname !== '/' ? target.pathname.replace(/\/$/, '') : '';
  const baseUrl = `${target.protocol}//127.0.0.1:${port}${basePath}`;
  tunnels.set(server.id, { process, baseUrl, signature });
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

  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), SERVER_REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(joinApiUrl(baseUrl, payload.path), { ...options, signal: controller.signal });
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
      throw new Error(data?.error || data?.message || `${response.status} ${response.statusText}`);
    }
    return { status: response.status, data };
  } finally {
    clearTimeout(timeout);
  }
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

function bufferFromIpcBinary(value) {
  if (Buffer.isBuffer(value)) return value;
  if (value instanceof ArrayBuffer) return Buffer.from(value);
  if (ArrayBuffer.isView(value)) return Buffer.from(value.buffer, value.byteOffset, value.byteLength);
  if (Array.isArray(value)) return Buffer.from(value);
  throw new Error('Invalid binary payload.');
}

async function gzipBinary(_event, payload) {
  return zlib.gzipSync(bufferFromIpcBinary(payload));
}

function showNotification(_event, payload = {}) {
  if (!Notification.isSupported()) {
    return { shown: false, reason: 'unsupported' };
  }
  const title = String(payload.title || 'Stellacode');
  const body = String(payload.body || '');
  const notification = new Notification({
    title,
    body,
    icon: appIconPath(),
    silent: Boolean(payload.silent)
  });
  notification.show();
  return { shown: true };
}

function parseTarEntries(buffer) {
  const entries = [];
  let offset = 0;
  let paxPath = '';
  while (offset + 512 <= buffer.length) {
    const header = buffer.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const name = header.toString('utf8', 0, 100).replace(/\0.*$/, '');
    const sizeRaw = header.toString('utf8', 124, 136).replace(/\0.*$/, '').trim();
    const size = Number.parseInt(sizeRaw || '0', 8) || 0;
    const type = String.fromCharCode(header[156] || 48);
    offset += 512;
    const data = buffer.subarray(offset, offset + size);
    offset += Math.ceil(size / 512) * 512;
    if (type === 'x') {
      const text = data.toString('utf8');
      const match = text.match(/path=([^\n]+)/);
      paxPath = match?.[1] || '';
      continue;
    }
    entries.push({
      name: paxPath || name,
      type,
      data: Buffer.from(data)
    });
    paxPath = '';
  }
  return entries;
}

async function uploadWorkspaceFile(_event, payload) {
  const settings = await readSettings();
  const server = settings.servers.find((item) => item.id === payload.serverId);
  if (!server) throw new Error(`Unknown server: ${payload.serverId}`);
  const baseUrl = await resolveServerBaseUrl(server);
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), SERVER_REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(joinApiUrl(
      baseUrl,
      `/api/conversations/${payload.conversationId}/workspace/upload?path=${encodeURIComponent(payload.path || '')}`
    ), {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${server.token}`,
        'Content-Type': 'application/gzip',
        Accept: 'application/json'
      },
      body: bufferFromIpcBinary(payload.data),
      signal: controller.signal
    });
    const text = await response.text();
    const data = text.trim() ? JSON.parse(text) : {};
    if (!response.ok) {
      throw new Error(data?.error || data?.message || `${response.status} ${response.statusText}`);
    }
    return data;
  } finally {
    clearTimeout(timeout);
  }
}

async function downloadWorkspaceFile(_event, payload) {
  const settings = await readSettings();
  const server = settings.servers.find((item) => item.id === payload.serverId);
  if (!server) throw new Error(`Unknown server: ${payload.serverId}`);
  const baseUrl = await resolveServerBaseUrl(server);
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), SERVER_REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(joinApiUrl(
      baseUrl,
      `/api/conversations/${payload.conversationId}/workspace/download?path=${encodeURIComponent(payload.path || '')}`
    ), {
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
    const archive = Buffer.from(await response.arrayBuffer());
    const basename = payload.suggestedName || path.basename(payload.path || '') || 'workspace';
    let saveName = payload.kind === 'file' ? basename : `${basename}.tar.gz`;
    let saveBuffer = archive;
    let filters = [{ name: 'Archive', extensions: ['tar.gz'] }];
    if (payload.kind === 'file') {
      const entries = parseTarEntries(zlib.gunzipSync(archive)).filter((entry) => entry.type !== '5');
      const first = entries[0];
      if (first) {
        saveName = path.basename(first.name || basename);
        saveBuffer = first.data;
        filters = [];
      }
    }
    const win = BrowserWindow.getFocusedWindow();
    const result = await dialog.showSaveDialog(win, {
      defaultPath: saveName,
      filters
    });
    if (result.canceled || !result.filePath) return { saved: false };
    await fs.writeFile(result.filePath, saveBuffer);
    return { saved: true, filePath: result.filePath, size: saveBuffer.length };
  } finally {
    clearTimeout(timeout);
  }
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1440,
    height: 920,
    minWidth: 960,
    minHeight: 680,
    title: 'Stellacode 2',
    icon: appIconPath(),
    titleBarStyle: process.platform === 'darwin' ? 'hiddenInset' : 'hidden',
    trafficLightPosition: { x: 18, y: 18 },
    backgroundColor: '#151515',
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false
    }
  });

  if (process.env.VITE_DEV_SERVER_URL) {
    mainWindow.loadURL(process.env.VITE_DEV_SERVER_URL);
  } else {
    mainWindow.loadFile(path.join(__dirname, '..', 'dist', 'index.html'));
  }

  mainWindow.webContents.on('console-message', (_event, details) => {
    if (details.level >= 2) {
      console.error(`[renderer:${details.level}] ${details.message} (${details.sourceId}:${details.lineNumber})`);
    }
  });
  mainWindow.webContents.on('render-process-gone', (_event, details) => {
    console.error('[renderer-gone]', details);
  });
}

function publishUpdaterState(patch) {
  updaterState = { ...updaterState, ...patch };
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send('updater:status', updaterState);
  }
}

async function checkForUpdatesNow() {
  if (!app.isPackaged) {
    publishUpdaterState({ state: 'disabled' });
    return updaterState;
  }
  if (['checking', 'downloading', 'downloaded'].includes(updaterState.state)) {
    return updaterState;
  }
  try {
    await autoUpdater.checkForUpdates();
  } catch (error) {
    console.warn('Auto-updater check failed:', error?.message || error);
    if (updaterState.state !== 'downloaded') {
      publishUpdaterState({
        state: 'error',
        error: error?.message || String(error)
      });
    }
  }
  return updaterState;
}

function setupAutoUpdater() {
  autoUpdater.autoDownload = true;
  autoUpdater.autoInstallOnAppQuit = true;

  autoUpdater.on('checking-for-update', () => {
    if (updaterState.state === 'downloaded') return;
    publishUpdaterState({ state: 'checking' });
  });
  autoUpdater.on('update-available', (info) => {
    publishUpdaterState({
      state: 'downloading',
      version: info.version,
      releaseDate: info.releaseDate,
      percent: 0
    });
  });
  autoUpdater.on('update-not-available', () => {
    if (updaterState.state === 'downloaded') return;
    publishUpdaterState({ state: 'idle', percent: 0 });
  });
  autoUpdater.on('download-progress', (progress) => {
    publishUpdaterState({
      state: 'downloading',
      percent: progress.percent,
      bytesPerSecond: progress.bytesPerSecond,
      transferred: progress.transferred,
      total: progress.total
    });
  });
  autoUpdater.on('update-downloaded', (info) => {
    publishUpdaterState({
      state: 'downloaded',
      version: info.version,
      percent: 100
    });
  });
  autoUpdater.on('error', (error) => {
    console.warn('Auto-updater error:', error?.message || error);
    if (updaterState.state === 'downloaded') return;
    publishUpdaterState({
      state: 'error',
      error: error?.message || String(error)
    });
  });

  if (!app.isPackaged) {
    publishUpdaterState({ state: 'disabled' });
    return;
  }

  checkForUpdatesNow();
  updateCheckTimer = setInterval(() => {
    checkForUpdatesNow();
  }, UPDATE_CHECK_INTERVAL_MS);
}

app.whenReady().then(() => {
  if (process.platform === 'darwin') {
    app.dock.setIcon(appIconPath());
  }
  ipcMain.handle('settings:load', readSettings);
  ipcMain.handle('settings:save', (_event, settings) => writeSettings(settings));
  ipcMain.handle('app:version', () => app.getVersion());
  ipcMain.handle('server:request', requestServer);
  ipcMain.handle('server:connectionInfo', serverConnectionInfo);
  ipcMain.handle('binary:gzip', gzipBinary);
  ipcMain.handle('app:notify', showNotification);
  ipcMain.handle('workspace:upload', uploadWorkspaceFile);
  ipcMain.handle('workspace:download', downloadWorkspaceFile);
  ipcMain.handle('updater:status', () => updaterState);
  ipcMain.handle('updater:check', () => checkForUpdatesNow());
  ipcMain.handle('updater:install', () => {
    if (updaterState.state === 'downloaded') {
      autoUpdater.quitAndInstall(false, true);
    }
    return updaterState;
  });
  createWindow();
  setupAutoUpdater();
});

app.on('window-all-closed', () => {
  if (updateCheckTimer) {
    clearInterval(updateCheckTimer);
    updateCheckTimer = null;
  }
  for (const serverId of Array.from(tunnels.keys())) {
    stopTunnel(serverId);
  }
  if (process.platform !== 'darwin') {
    app.quit();
  }
});

app.on('activate', () => {
  if (BrowserWindow.getAllWindows().length === 0) {
    createWindow();
  }
});
