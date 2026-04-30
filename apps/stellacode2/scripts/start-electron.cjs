const childProcess = require('node:child_process');
const http = require('node:http');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const devUrl = 'http://127.0.0.1:5175';

function spawn(command, args, options = {}) {
  return childProcess.spawn(command, args, {
    cwd: root,
    stdio: 'inherit',
    shell: process.platform === 'win32',
    ...options
  });
}

function waitForServer(url, timeoutMs = 20000) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      const request = http.get(url, (response) => {
        response.resume();
        resolve();
      });
      request.on('error', () => {
        if (Date.now() - started > timeoutMs) {
          reject(new Error(`Timed out waiting for ${url}`));
          return;
        }
        setTimeout(tick, 150);
      });
      request.setTimeout(1000, () => {
        request.destroy();
      });
    };
    tick();
  });
}

async function main() {
  const vite = spawn('npx', ['vite', '--host', '127.0.0.1', '--port', '5175', '--strictPort']);
  const shutdown = () => {
    vite.kill('SIGTERM');
  };
  process.once('SIGINT', shutdown);
  process.once('SIGTERM', shutdown);

  await waitForServer(devUrl);
  const electron = spawn('npx', ['electron', '.'], {
    env: {
      ...process.env,
      VITE_DEV_SERVER_URL: devUrl
    }
  });
  electron.on('exit', (code) => {
    shutdown();
    process.exit(code ?? 0);
  });
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
