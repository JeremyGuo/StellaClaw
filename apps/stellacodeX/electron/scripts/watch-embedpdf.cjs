const childProcess = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const electronRoot = path.resolve(__dirname, '..');
const repoRoot = path.resolve(__dirname, '../../../..');
const embedPdfRoot = path.resolve(repoRoot, '3rd_party/embed-pdf-viewer');

const watchTargets = [
  { kind: 'models', dir: 'packages/models/src' },
  { kind: 'pdfium', dir: 'packages/pdfium/src' },
  { kind: 'engines', dir: 'packages/engines/src' },
  { kind: 'engines', dir: 'packages/engines/tools' },
  { kind: 'engines', dir: 'packages/engines/vite.config.ts' }
].map((target) => ({
  ...target,
  path: path.resolve(embedPdfRoot, target.dir)
}));

let timer = null;
let running = false;
let rerun = false;
const pendingKinds = new Set();
const watchers = [];

function log(message) {
  console.log(`[embedpdf-watch] ${message}`);
}

function run(command, args, cwd = embedPdfRoot) {
  childProcess.execFileSync(command, args, {
    cwd,
    stdio: 'inherit',
    env: {
      ...process.env,
      NPM_TOKEN: process.env.NPM_TOKEN || ''
    }
  });
}

function ensurePrepared() {
  run('node', ['scripts/prepare-embedpdf.cjs'], electronRoot);
}

function schedule(kind, detail) {
  if (kind === 'models') {
    pendingKinds.add('models');
    pendingKinds.add('engines');
  } else if (kind === 'pdfium') {
    pendingKinds.add('pdfium');
    pendingKinds.add('engines');
  } else {
    pendingKinds.add('engines');
  }

  if (detail) log(`change detected: ${detail}`);
  if (running) {
    rerun = true;
    return;
  }

  if (timer) clearTimeout(timer);
  timer = setTimeout(rebuild, 250);
}

function rebuild() {
  timer = null;
  if (running) {
    rerun = true;
    return;
  }

  const kinds = new Set(pendingKinds);
  pendingKinds.clear();
  if (kinds.size === 0) return;

  running = true;
  rerun = false;
  const startedAt = Date.now();
  log(`rebuilding ${Array.from(kinds).join(', ')}`);

  try {
    if (kinds.has('models')) {
      run('pnpm', ['--filter', '@embedpdf/models', 'build']);
    }
    if (kinds.has('pdfium')) {
      run('pnpm', ['--filter', '@embedpdf/pdfium', 'build']);
    }
    if (kinds.has('engines')) {
      run('pnpm', ['--filter', '@embedpdf/engines', 'build:base']);
    }
    log(`rebuild completed in ${Date.now() - startedAt}ms`);
  } catch (error) {
    log(`rebuild failed: ${error?.message || error}`);
  } finally {
    running = false;
    if (rerun || pendingKinds.size > 0) {
      rerun = false;
      setTimeout(rebuild, 250);
    }
  }
}

function watchDirectory(target) {
  if (!fs.existsSync(target.path)) return;
  if (fs.statSync(target.path).isFile()) {
    const watcher = fs.watch(target.path, { persistent: true }, () => {
      schedule(target.kind, path.relative(embedPdfRoot, target.path));
    });
    watchers.push(watcher);
    return;
  }

  const watcher = fs.watch(target.path, { persistent: true, recursive: true }, (_event, filename) => {
    if (!filename) return;
    const name = String(filename);
    if (name.includes('/dist/') || name.includes('/node_modules/')) return;
    if (!/\.(ts|tsx|js|mjs|cjs|json)$/.test(name)) return;
    schedule(target.kind, path.join(path.relative(embedPdfRoot, target.path), name));
  });
  watchers.push(watcher);
}

try {
  ensurePrepared();
  for (const target of watchTargets) watchDirectory(target);
  log('watching EmbedPDF sources');
} catch (error) {
  console.error(error);
  process.exit(1);
}

function shutdown() {
  for (const watcher of watchers) watcher.close();
  process.exit(0);
}

process.once('SIGINT', shutdown);
process.once('SIGTERM', shutdown);
