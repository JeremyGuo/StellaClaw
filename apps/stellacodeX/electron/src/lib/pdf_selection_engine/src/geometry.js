export function clamp(value, min, max) {
  if (!Number.isFinite(value)) return min;
  return Math.max(min, Math.min(max, value));
}

export function normalizeRect(input) {
  if (!input) return null;
  const x = numberFrom(input.x ?? input.left ?? input.origin?.x ?? input.tightX);
  const y = numberFrom(input.y ?? input.top ?? input.origin?.y ?? input.tightY);
  const width = numberFrom(input.width ?? input.size?.width ?? input.tightWidth);
  const height = numberFrom(input.height ?? input.size?.height ?? input.tightHeight);
  if (!(width > 0) || !(height > 0)) return null;
  return {
    left: x,
    top: y,
    width,
    height,
    right: x + width,
    bottom: y + height
  };
}

export function rectCenter(rect) {
  return {
    x: rect.left + rect.width / 2,
    y: rect.top + rect.height / 2
  };
}

export function rectIntersects(a, b) {
  return Boolean(
    a && b
    && a.left < b.right
    && a.right > b.left
    && a.top < b.bottom
    && a.bottom > b.top
  );
}

export function rectUnion(rects) {
  let left = Infinity;
  let top = Infinity;
  let right = -Infinity;
  let bottom = -Infinity;
  let count = 0;
  for (const rect of rects) {
    if (!rect) continue;
    left = Math.min(left, rect.left);
    top = Math.min(top, rect.top);
    right = Math.max(right, rect.right);
    bottom = Math.max(bottom, rect.bottom);
    count += 1;
  }
  if (count === 0) return null;
  return {
    left,
    top,
    right,
    bottom,
    width: right - left,
    height: bottom - top
  };
}

export function normalizeGlyphPage(page, pageIndex) {
  const pageNumber = Number(page.pageNumber ?? page.page_num ?? page.index + 1 ?? pageIndex + 1) || pageIndex + 1;
  const width = numberFrom(page.width ?? page.size?.width ?? page.pageWidth);
  const height = numberFrom(page.height ?? page.size?.height ?? page.pageHeight);
  const glyphs = [];

  for (const [localIndex, raw] of (page.glyphs || []).entries()) {
    const char = String(raw.char ?? raw.text ?? '');
    if (!char) continue;

    const looseRect = normalizeRect(raw.rect || raw.looseRect || raw.loose || raw);
    const tightRect = normalizeRect(raw.tightRect || raw.tight || {
      x: raw.tightX,
      y: raw.tightY,
      width: raw.tightWidth,
      height: raw.tightHeight
    });
    const rect = looseRect || tightRect;
    if (!rect) continue;
    const lineRect = tightRect || rect;

    glyphs.push({
      id: raw.id || `${pageNumber}:${localIndex}`,
      char,
      pageNumber,
      pageIndex: pageIndex,
      localIndex,
      charIndex: Number.isFinite(raw.charIndex) ? raw.charIndex : localIndex,
      rect,
      tightRect,
      hitRect: tightRect || rect,
      isSpace: Boolean(raw.isSpace) || /^\s$/.test(char),
      isEmpty: Boolean(raw.isEmpty),
      width: rect.width,
      height: rect.height,
      left: rect.left,
      right: rect.right,
      top: rect.top,
      bottom: rect.bottom,
      centerX: rect.left + rect.width / 2,
      centerY: rect.top + rect.height / 2,
      lineTop: lineRect.top,
      lineBottom: lineRect.bottom,
      lineHeight: lineRect.height,
      lineCenterY: rect.top + rect.height / 2,
      pageWidth: width,
      pageHeight: height
    });
  }

  return {
    pageNumber,
    pageIndex,
    width,
    height,
    text: String(page.text || ''),
    glyphs
  };
}

export function median(values, fallback = 0) {
  const sorted = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (sorted.length === 0) return fallback;
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

function numberFrom(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : 0;
}
