import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const rootDir = path.resolve(__dirname, '../../..');
const embedPdfRoot = path.resolve(rootDir, '3rd_party/embed-pdf-viewer/packages');

export default defineConfig({
  base: './',
  plugins: [react()],
  resolve: {
    alias: [
      {
        find: /^@embedpdf\/engines\/pdfium-direct-engine$/,
        replacement: path.resolve(embedPdfRoot, 'engines/dist/lib/pdfium/web/direct-engine.js')
      },
      {
        find: /^@embedpdf\/engines\/pdfium-worker-engine$/,
        replacement: path.resolve(embedPdfRoot, 'engines/dist/lib/pdfium/web/worker-engine.js')
      },
      {
        find: /^@embedpdf\/engines$/,
        replacement: path.resolve(embedPdfRoot, 'engines/dist/index.js')
      },
      {
        find: /^@embedpdf\/pdfium\/pdfium\.wasm$/,
        replacement: path.resolve(embedPdfRoot, 'pdfium/dist/pdfium.wasm')
      },
      {
        find: /^@embedpdf\/pdfium$/,
        replacement: path.resolve(embedPdfRoot, 'pdfium/dist/index.browser.js')
      },
      {
        find: /^@embedpdf\/models$/,
        replacement: path.resolve(embedPdfRoot, 'models/dist/index.js')
      }
    ]
  },
  server: {
    host: '127.0.0.1',
    port: 5175,
    strictPort: true,
    fs: {
      allow: [rootDir]
    }
  },
  optimizeDeps: {
    exclude: ['@embedpdf/engines', '@embedpdf/pdfium', '@embedpdf/models']
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true
  }
});
