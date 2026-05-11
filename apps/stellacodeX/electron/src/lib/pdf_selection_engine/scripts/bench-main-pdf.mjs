#!/usr/bin/env node
import { execFile } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { promisify } from 'node:util';
import { PdfSelectionIndex } from '../src/index.js';

const execFileAsync = promisify(execFile);
const pdfPath = expandHome(process.argv[2] || '~/Downloads/main.pdf');
const maxPages = Number(process.argv[3] || 6);

const started = performance.now();
const pages = await popplerTsvToGlyphPages(pdfPath, maxPages);
const extractedAt = performance.now();
const index = PdfSelectionIndex.fromPages(pages);
const indexedAt = performance.now();
const selections = runSyntheticSelections(index);
const selectedAt = performance.now();

console.log(JSON.stringify({
  pdfPath,
  maxPages,
  pages: pages.length,
  stats: index.stats(),
  extractMs: round(extractedAt - started),
  indexMs: round(indexedAt - extractedAt),
  selectionMs: round(selectedAt - indexedAt),
  selections
}, null, 2));

async function popplerTsvToGlyphPages(path, pages) {
  const { stdout } = await execFileAsync('pdftotext', ['-f', '1', '-l', String(pages), '-tsv', path, '-'], {
    maxBuffer: 64 * 1024 * 1024
  });
  return parsePopplerTsv(stdout);
}

function parsePopplerTsv(tsv) {
  const lines = tsv.split(/\r?\n/).filter(Boolean);
  const header = lines.shift()?.split('\t') || [];
  const pages = new Map();
  for (const line of lines) {
    const fields = line.split('\t');
    const row = Object.fromEntries(header.map((key, index) => [key, fields[index] ?? '']));
    const pageNumber = Number(row.page_num);
    if (!pageNumber) continue;
    if (!pages.has(pageNumber)) {
      pages.set(pageNumber, { pageNumber, width: 0, height: 0, glyphs: [] });
    }
    const page = pages.get(pageNumber);
    const left = Number(row.left);
    const top = Number(row.top);
    const width = Number(row.width);
    const height = Number(row.height);
    if (row.level === '1') {
      page.width = width;
      page.height = height;
      continue;
    }
    if (row.level !== '5' || !row.text || row.text.startsWith('###')) continue;
    page.glyphs.push(...wordToGlyphs(row.text, left, top, width, height));
  }
  return Array.from(pages.values()).filter((page) => page.glyphs.length > 0);
}

function wordToGlyphs(text, left, top, width, height) {
  const chars = Array.from(text);
  const weights = chars.map(charWeight);
  const total = weights.reduce((sum, value) => sum + value, 0) || chars.length || 1;
  const glyphs = [];
  let cursor = left;
  for (const [index, char] of chars.entries()) {
    const glyphWidth = width * (weights[index] / total);
    glyphs.push({
      char,
      rect: { left: cursor, top, width: glyphWidth, height },
      tightRect: {
        left: cursor + glyphWidth * 0.08,
        top: top + height * 0.12,
        width: Math.max(0.5, glyphWidth * 0.84),
        height: Math.max(0.5, height * 0.76)
      }
    });
    cursor += glyphWidth;
  }
  return glyphs;
}

function runSyntheticSelections(index) {
  const glyphs = index.glyphs;
  if (glyphs.length < 2) return { count: 0 };
  const count = Math.min(2000, Math.max(100, glyphs.length));
  let totalChars = 0;
  let misses = 0;
  const started = performance.now();
  for (let i = 0; i < count; i += 1) {
    const a = glyphs[(i * 17) % glyphs.length];
    const b = glyphs[Math.min(glyphs.length - 1, ((i * 17) % glyphs.length) + 24)];
    const anchor = index.hitTest({ pageNumber: a.pageNumber, x: a.left, y: a.centerY });
    const focus = index.hitTest({ pageNumber: b.pageNumber, x: b.right, y: b.centerY });
    const selection = index.selectBetween(anchor, focus);
    if (!selection) {
      misses += 1;
      continue;
    }
    totalChars += selection.selectedText.length;
  }
  const elapsedMs = performance.now() - started;
  return {
    count,
    misses,
    totalChars,
    elapsedMs: round(elapsedMs),
    selectionsPerMs: round(count / Math.max(0.001, elapsedMs))
  };
}

function charWeight(char) {
  if (/[\u4e00-\u9fff\u3040-\u30ff\uac00-\ud7af]/.test(char)) return 1;
  if (/\s/.test(char)) return 0.32;
  if (/[ilI.,:;!|]/.test(char)) return 0.35;
  if (/[mwMW@#%&]/.test(char)) return 0.95;
  return 0.62;
}

function expandHome(path) {
  if (!path.startsWith('~/')) return path;
  return `${process.env.HOME}${path.slice(1)}`;
}

function round(value) {
  return Math.round(value * 100) / 100;
}
