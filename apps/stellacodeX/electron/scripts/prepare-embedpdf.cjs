const childProcess = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const repoRoot = path.resolve(__dirname, '../../../..');
const embedPdfRoot = path.resolve(repoRoot, '3rd_party/embed-pdf-viewer');

const requiredFiles = [
  'packages/engines/dist/lib/pdfium/web/direct-engine.js',
  'packages/engines/dist/lib/pdfium/web/worker-engine.js',
  'packages/models/dist/index.js',
  'packages/pdfium/dist/index.browser.js',
  'packages/pdfium/dist/pdfium.wasm'
].map((file) => path.resolve(embedPdfRoot, file));

function run(command, args, options = {}) {
  childProcess.execFileSync(command, args, {
    cwd: embedPdfRoot,
    stdio: 'inherit',
    shell: process.platform === 'win32',
    ...options
  });
}

function hasRequiredBuild() {
  return requiredFiles.every((file) => fs.existsSync(file));
}

if (!fs.existsSync(path.join(embedPdfRoot, 'package.json'))) {
  console.error('Missing 3rd_party/embed-pdf-viewer. Run git submodule update --init --recursive.');
  process.exit(1);
}

if (hasRequiredBuild() && process.env.STELLA_REBUILD_EMBEDPDF !== '1') {
  process.exit(0);
}

if (!fs.existsSync(path.join(embedPdfRoot, 'node_modules'))) {
  run('pnpm', ['install']);
}

run('pnpm', ['--filter', '@embedpdf/build', 'build']);
run('pnpm', ['--filter', '@embedpdf/fonts-*', 'build']);
run('pnpm', ['--filter', '@embedpdf/models', '--filter', '@embedpdf/pdfium', 'build']);
run('pnpm', ['--filter', '@embedpdf/engines', 'build:base']);
