import { normalizeRect } from './geometry.js';

export function glyphPagesFromEmbedPdf(pages) {
  return pages.map((page, index) => glyphPageFromEmbedPdf({ ...page, pageIndex: page.pageIndex ?? index }));
}

export function glyphPageFromEmbedPdf({ page, geometry, textRuns, pageIndex = 0, scale = 1 }) {
  const sourcePage = page || {};
  const pageNumber = Number(sourcePage.pageNumber ?? sourcePage.index + 1 ?? pageIndex + 1) || pageIndex + 1;
  const pageWidth = Number(sourcePage.width ?? sourcePage.size?.width ?? sourcePage.rotatedSize?.width ?? 0) * scale;
  const pageHeight = Number(sourcePage.height ?? sourcePage.size?.height ?? sourcePage.rotatedSize?.height ?? 0) * scale;
  const charsByIndex = mapCharsByIndex(textRuns?.runs || []);
  const glyphs = [];

  for (const run of geometry?.runs || []) {
    const charStart = Number(run.charStart || 0);
    for (const [offset, glyph] of (run.glyphs || []).entries()) {
      const charIndex = charStart + offset;
      const char = charsByIndex.get(charIndex) ?? '';
      if (!char) continue;
      const looseRect = scaleGlyphRect({
        x: glyph.x,
        y: glyph.y,
        width: glyph.width,
        height: glyph.height
      }, scale);
      const tightRect = scaleGlyphRect({
        x: glyph.tightX,
        y: glyph.tightY,
        width: glyph.tightWidth,
        height: glyph.tightHeight
      }, scale);
      glyphs.push({
        id: `${pageNumber}:${charIndex}`,
        char,
        charIndex,
        rect: looseRect,
        tightRect,
        isSpace: Boolean(glyph.flags & 1) || /^\s$/.test(char),
        isEmpty: Boolean(glyph.flags & 2)
      });
    }
  }

  return {
    pageNumber,
    pageIndex,
    width: pageWidth,
    height: pageHeight,
    text: textFromRuns(textRuns?.runs || []),
    glyphs
  };
}

function mapCharsByIndex(textRuns) {
  const map = new Map();
  for (const run of textRuns) {
    const start = Number(run.charIndex || 0);
    const chars = Array.from(String(run.text || ''));
    for (const [offset, char] of chars.entries()) {
      map.set(start + offset, char);
    }
  }
  return map;
}

function textFromRuns(textRuns) {
  let text = '';
  for (const run of textRuns || []) {
    const start = Number(run.charIndex || 0);
    const runText = String(run.text || '');
    if (!runText) continue;
    if (start > text.length) text += ' '.repeat(start - text.length);
    text = text.slice(0, start) + runText + text.slice(start + runText.length);
  }
  return text;
}

function scaleGlyphRect(rect, scale) {
  const normalized = normalizeRect(rect);
  if (!normalized) return null;
  return {
    left: normalized.left * scale,
    top: normalized.top * scale,
    width: normalized.width * scale,
    height: normalized.height * scale
  };
}
