import {
  median,
  normalizeGlyphPage,
  normalizeRect,
  rectCenter
} from './geometry.js';

export const PDF_SELECTION_ENGINE_VERSION = 16;

const DEFAULT_BIN_SIZE = 24;

export class PdfSelectionIndex {
  constructor(pages, options = {}) {
    this.options = {
      binSize: options.binSize || DEFAULT_BIN_SIZE,
      hitPadding: options.hitPadding ?? 4,
      lineMergeTolerance: options.lineMergeTolerance ?? 0.55,
      segmentGapFactor: options.segmentGapFactor ?? 1.25,
      sameLaneSelection: options.sameLaneSelection ?? true,
      ...options
    };
    this.pages = pages
      .map((page, index) => normalizeGlyphPage(page, index))
      .sort((a, b) => a.pageNumber - b.pageNumber);
    this.glyphs = [];
    this.pageMap = new Map();
    this.pageGlyphs = new Map();
    this.lineGlyphs = new Map();
    this.pageLineEntries = new Map();
    this.pageBlocks = new Map();
    this.engineVersion = PDF_SELECTION_ENGINE_VERSION;
    this.orderGlyphs();
    this.buildSpatialIndex();
  }

  static fromPages(pages, options) {
    return new PdfSelectionIndex(pages, options);
  }

  stats() {
    return {
      pages: this.pages.length,
      glyphs: this.glyphs.length,
      bins: Array.from(this.pageMap.values()).reduce((sum, page) => sum + page.bins.size, 0)
    };
  }

  hitTest(point, options = {}) {
    const pageNumber = Number(point?.pageNumber ?? point?.page);
    const page = this.pageMap.get(pageNumber);
    if (!page) return null;

    const x = Number(point.x);
    const y = Number(point.y);
    if (!Number.isFinite(x) || !Number.isFinite(y)) return null;

    const candidates = this.candidatesForPoint(page, x, y, options.lane);
    const containingGlyphs = candidates.filter((glyph) => pointInsideRect(x, y, glyph.hitRect || glyph.rect));
    const lineBoundary = boundaryForBestLineEdge(candidates, x, y, containingGlyphs);
    if (lineBoundary) {
      return {
        pageNumber,
        x,
        y,
        glyph: lineBoundary.glyph,
        boundary: lineBoundary.boundary,
        affinity: lineBoundary.boundary === lineBoundary.glyph.order ? 'before' : 'after',
        lane: selectionLaneForGlyph(lineBoundary.glyph)
      };
    }
    let best = null;
    const scoredGlyphs = containingGlyphs.length > 0 ? containingGlyphs : candidates;
    for (const glyph of scoredGlyphs) {
      const hitRect = glyph.hitRect || glyph.rect;
      const expanded = expandRect(hitRect, Math.max(this.options.hitPadding, glyph.height * 0.25));
      const dx = x < expanded.left ? expanded.left - x : x > expanded.right ? x - expanded.right : 0;
      const dy = y < expanded.top ? expanded.top - y : y > expanded.bottom ? y - expanded.bottom : 0;
      const center = rectCenter(hitRect);
      const insideBonus = pointInsideRect(x, y, hitRect) ? -120 : 0;
      const score = Math.abs(y - center.y) * 100 + dy * 20 + dx + insideBonus;
      if (!best || score < best.score) best = { glyph, score };
    }

    if (!best) return null;
    const center = rectCenter(best.glyph.hitRect || best.glyph.rect);
    const boundary = x <= center.x ? best.glyph.order : best.glyph.order + 1;
    return {
      pageNumber,
      x,
      y,
      glyph: best.glyph,
      boundary,
      affinity: x <= center.x ? 'before' : 'after',
      lane: selectionLaneForGlyph(best.glyph)
    };
  }

  selectBetween(anchor, focus, options = {}) {
    if (!anchor || !focus) return null;
    const sameLaneSelection = options.sameLaneSelection ?? this.options.sameLaneSelection;
    if (sameLaneSelection && options.focusPoint) {
      const sameLineSelection = this.selectWithinAnchorLine(anchor, options.focusPoint, {
        force: Boolean(options.forceAnchorLine)
      });
      if (sameLineSelection) return sameLineSelection;
    }
    let effectiveFocus = focus;
    let lane = null;
    if (sameLaneSelection && anchor.pageNumber === focus.pageNumber) {
      lane = anchor.lane?.laneIndex >= 0 ? anchor.lane : null;
      if (lane && !glyphMatchesLane(focus.glyph, lane) && options.focusPoint) {
        effectiveFocus = this.hitTest(options.focusPoint, { lane }) || focus;
      }
    }
    if (sameLaneSelection && options.focusPoint) {
      effectiveFocus = this.snapFocusAcrossLineGutter(anchor, effectiveFocus, options.focusPoint) || effectiveFocus;
    }
    const start = Math.min(anchor.boundary, effectiveFocus.boundary);
    const end = Math.max(anchor.boundary, effectiveFocus.boundary);
    if (end <= start) return null;
    const glyphs = [];
    for (let order = start; order < end && order < this.glyphs.length; order += 1) {
      const glyph = this.glyphs[order];
      if (glyph && (!lane || glyphBelongsToLane(glyph, lane))) glyphs.push(glyph);
    }
    return this.selectionFromGlyphs(completeInteriorLineGlyphs(glyphs, this.lineGlyphs), { ...options, ordered: true });
  }

  snapFocusAcrossLineGutter(anchor, focus, focusPoint) {
    if (!anchor?.glyph || !focus?.glyph || !focusPoint || anchor.pageNumber !== focus.pageNumber) return null;
    const focusLine = this.lineGlyphs.get(lineKeyForGlyph(focus.glyph));
    const anchorLineKey = lineKeyForGlyph(anchor.glyph);
    if (!focusLine || !anchorLineKey || focusLine.key === anchorLineKey) return null;
    const laneKey = laneLineKey(focus.glyph.pageNumber, focus.glyph.laneIndex);
    const lines = this.pageLineEntries.get(laneKey);
    const lineIndex = focusLine.indexInLane;
    if (!lines || !Number.isInteger(lineIndex)) return null;
    const y = Number(focusPoint.y);
    if (!Number.isFinite(y)) return null;
    const threshold = Math.max(2, Math.min(8, focusLine.height * 0.35));

    if (focus.boundary > anchor.boundary && y < focusLine.top + threshold) {
      const previous = lines[lineIndex - 1];
      const last = previous?.glyphs?.at(-1);
      if (!last) return null;
      return {
        pageNumber: last.pageNumber,
        x: Number(focusPoint.x),
        y,
        glyph: last,
        boundary: last.order + 1,
        affinity: 'after',
        lane: selectionLaneForGlyph(last)
      };
    }

    if (focus.boundary < anchor.boundary && y > focusLine.bottom - threshold) {
      const next = lines[lineIndex + 1];
      const first = next?.glyphs?.[0];
      if (!first) return null;
      return {
        pageNumber: first.pageNumber,
        x: Number(focusPoint.x),
        y,
        glyph: first,
        boundary: first.order,
        affinity: 'before',
        lane: selectionLaneForGlyph(first)
      };
    }
    return null;
  }

  selectWithinAnchorLine(anchor, focusPoint, options = {}) {
    if (!anchor?.glyph || !focusPoint) return null;
    const lineEntry = this.lineGlyphs.get(lineKeyForGlyph(anchor.glyph));
    const lineGlyphs = lineEntry?.glyphs || [];
    if (lineGlyphs.length === 0) return null;
    const top = lineEntry.top;
    const bottom = lineEntry.bottom;
    const height = Math.max(1, bottom - top);
    const lineBand = Math.max(3, height * 0.65);
    const y = Number(focusPoint.y);
    if (!options.force && (!Number.isFinite(y) || y < top - lineBand || y > bottom + lineBand)) return null;
    const focusBoundary = boundaryForOrderedLine(lineGlyphs, Number(focusPoint.x));
    if (!focusBoundary) return null;
    const start = Math.min(anchor.boundary, focusBoundary);
    const end = Math.max(anchor.boundary, focusBoundary);
    if (end <= start) return null;
    const glyphs = [];
    for (const glyph of lineGlyphs) {
      if (glyph.order >= start && glyph.order < end) glyphs.push(glyph);
    }
    return this.selectionFromGlyphs(glyphs, { ...options, ordered: true });
  }

  selectArea(area) {
    const pageNumber = Number(area?.pageNumber ?? area?.page);
    const page = this.pageMap.get(pageNumber);
    if (!page) return null;
    const rect = normalizeRect(area);
    if (!rect) return null;
    const selected = page.glyphs.filter((glyph) => {
      const center = rectCenter(glyph.hitRect || glyph.rect);
      return center.x >= rect.left && center.x <= rect.right && center.y >= rect.top && center.y <= rect.bottom;
    });
    return this.selectionFromGlyphs(selected);
  }

  orderGlyphs() {
    const ordered = [];
    for (const page of this.pages) {
      const pageGlyphs = page.glyphs.filter((glyph) => !glyph.isEmpty);
      const lines = buildLines(pageGlyphs, this.options);
      const segments = buildSegments(lines, this.options, page.width);
      const ignoredSegments = new Set(segments.filter((segment) => isMarginNumberSegment(segment, page.width)).map((segment) => segment.id));
      const lanes = buildLanes(segments, page.width);
      const segmentLane = new Map();
      const spanningSegments = new Set(segments.filter((segment) => isSpanningSegment(segment, page.width)).map((segment) => segment.id));
      for (const lane of lanes) {
        for (const segment of lane.segments) {
          segmentLane.set(segment.id, lane.index);
        }
      }
      for (const segment of segments) {
        if (ignoredSegments.has(segment.id) || segment.line?.isTableLike || spanningSegments.has(segment.id)) {
          segment.laneIndex = -1;
        } else {
          segment.laneIndex = segmentLane.get(segment.id) ?? 0;
        }
      }
      const blocks = buildLayoutBlocks(segments, page.width, page.height);
      this.pageBlocks.set(page.pageNumber, blocks);

      const selectableGlyphs = pageGlyphs.filter((glyph) => !ignoredSegments.has(glyph.segment?.id));
      const pageOrdered = selectableGlyphs.sort((a, b) => {
        const segA = a.segment;
        const segB = b.segment;
        const laneA = segA?.laneIndex ?? 0;
        const laneB = segB?.laneIndex ?? 0;
        const blockA = segA?.block;
        const blockB = segB?.block;
        if (blockA && blockB && blockA !== blockB && blockA.readingIndex !== blockB.readingIndex) {
          return blockA.readingIndex - blockB.readingIndex;
        }
        if ((laneA < 0 || laneB < 0) && Math.abs(a.line.top - b.line.top) > 1.5) {
          return a.line.top - b.line.top;
        }
        if (laneA !== laneB) return laneA - laneB;
        if (Math.abs(a.line.top - b.line.top) > 1.5) return a.line.top - b.line.top;
        if (Math.abs((segA?.left ?? a.left) - (segB?.left ?? b.left)) > 1.5) {
          return (segA?.left ?? a.left) - (segB?.left ?? b.left);
        }
        return a.left - b.left || a.localIndex - b.localIndex;
      });

      for (const glyph of pageOrdered) {
        const segment = glyph.segment;
        const laneIndex = segment?.laneIndex ?? 0;
        const lane = lanes.find((candidate) => candidate.index === laneIndex);
        ordered.push({
          ...glyph,
          laneIndex,
          laneLeft: lane?.left ?? segment?.left ?? glyph.left,
          laneRight: lane?.right ?? segment?.right ?? glyph.right
        });
      }
    }
    this.glyphs = ordered.map((glyph, order) => ({ ...glyph, order }));
    this.pageGlyphs.clear();
    this.lineGlyphs.clear();
    this.pageLineEntries.clear();
    for (const glyph of this.glyphs) {
      const pageGlyphs = this.pageGlyphs.get(glyph.pageNumber);
      if (pageGlyphs) {
        pageGlyphs.push(glyph);
      } else {
        this.pageGlyphs.set(glyph.pageNumber, [glyph]);
      }

      const lineKey = lineKeyForGlyph(glyph);
      if (!lineKey) continue;
      const lineTop = glyph.line?.top ?? glyph.lineTop ?? glyph.top;
      const lineBottom = glyph.line?.bottom ?? glyph.lineBottom ?? glyph.bottom;
      const lineHeight = glyph.line?.height ?? glyph.lineHeight ?? glyph.height;
      let lineEntry = this.lineGlyphs.get(lineKey);
      if (!lineEntry) {
        lineEntry = {
          key: lineKey,
          glyphs: [],
          pageNumber: glyph.pageNumber,
          laneIndex: glyph.laneIndex ?? -1,
          lineId: glyph.line?.id,
          blockId: glyph.block?.id ?? '',
          blockType: glyph.block?.type ?? '',
          tableLike: Boolean(glyph.line?.isTableLike),
          top: lineTop,
          bottom: lineBottom,
          centerY: lineCenterY(glyph.line) || glyph.lineCenterY || glyph.centerY,
          height: lineHeight
        };
        this.lineGlyphs.set(lineKey, lineEntry);
      }
      lineEntry.glyphs.push(glyph);
      lineEntry.top = Math.min(lineEntry.top, lineTop);
      lineEntry.bottom = Math.max(lineEntry.bottom, lineBottom);
      lineEntry.height = Math.max(lineEntry.height, lineBottom - lineTop, lineHeight);
      lineEntry.centerY = lineCenterY(lineEntry);
      lineEntry.tableLike = lineEntry.tableLike || Boolean(glyph.line?.isTableLike);
      if (!lineEntry.blockId && glyph.block?.id) {
        lineEntry.blockId = glyph.block.id;
        lineEntry.blockType = glyph.block.type;
      }
    }
    for (const lineEntry of this.lineGlyphs.values()) {
      lineEntry.glyphs.sort((a, b) => a.left - b.left || a.localIndex - b.localIndex);
      const key = laneLineKey(lineEntry.pageNumber, lineEntry.laneIndex);
      const entries = this.pageLineEntries.get(key);
      if (entries) {
        entries.push(lineEntry);
      } else {
        this.pageLineEntries.set(key, [lineEntry]);
      }
    }
    for (const entries of this.pageLineEntries.values()) {
      entries.sort((a, b) => a.top - b.top || a.glyphs[0]?.left - b.glyphs[0]?.left);
      entries.forEach((entry, index) => {
        entry.indexInLane = index;
      });
    }
  }

  buildSpatialIndex() {
    this.pageMap.clear();
    for (const page of this.pages) {
      const glyphs = this.pageGlyphs.get(page.pageNumber) || [];
      const binSize = this.options.binSize || DEFAULT_BIN_SIZE;
      const bins = new Map();
      for (const glyph of glyphs) {
        const rect = glyph.hitRect || glyph.rect;
        const start = Math.floor(rect.top / binSize);
        const end = Math.floor(rect.bottom / binSize);
        for (let bin = start; bin <= end; bin += 1) {
          if (!bins.has(bin)) bins.set(bin, []);
          bins.get(bin).push(glyph);
        }
      }
      this.pageMap.set(page.pageNumber, {
        ...page,
        glyphs,
        bins,
        binSize,
        lanes: lanesFromGlyphs(glyphs),
        blocks: this.pageBlocks.get(page.pageNumber) || []
      });
    }
  }

  candidatesForPoint(page, x, y, lane = null) {
    const bin = Math.floor(y / page.binSize);
    const candidates = new Set();
    for (let offset = -1; offset <= 1; offset += 1) {
      for (const glyph of page.bins.get(bin + offset) || []) candidates.add(glyph);
    }
    const effectiveLane = lane || laneForPoint(page, x);
    const scoped = [];
    for (const glyph of candidates) {
      if (!effectiveLane || glyphMatchesLane(glyph, effectiveLane)) scoped.push(glyph);
    }
    if (scoped.length > 0) return scoped;
    let fallback = page.glyphs;
    if (effectiveLane) {
      fallback = [];
      for (const glyph of page.glyphs) {
        if (glyphMatchesLane(glyph, effectiveLane)) fallback.push(glyph);
      }
    }
    return fallback.length > 0 ? fallback : page.glyphs;
  }

  selectionFromGlyphs(glyphs, options = {}) {
    const selected = options.ordered ? glyphs : [...glyphs].sort((a, b) => a.order - b.order);
    if (selected.length === 0) return null;
    const includeText = options.includeText ?? true;
    const pages = [];
    let previousPage = null;
    for (const glyph of selected) {
      if (glyph.pageNumber === previousPage) continue;
      previousPage = glyph.pageNumber;
      pages.push(glyph.pageNumber);
    }
    const selectedText = includeText ? buildSelectedText(selected, this.pageMap) : '';
    return {
      selectedText,
      rects: buildSelectionRects(selected, this.lineGlyphs),
      glyphCount: selected.length,
      pages,
      start: selected[0].order,
      end: selected.at(-1).order + 1,
      textReady: includeText
    };
  }
}

function buildLines(glyphs, options) {
  const medianHeight = median(glyphs.map((glyph) => glyph.lineHeight ?? glyph.height), 8);
  const tolerance = Math.max(1.2, medianHeight * Math.min(options.lineMergeTolerance, 0.38));
  const lines = [];
  const metrics = new Map(glyphs.map((glyph) => [glyph, glyphLineMetrics(glyph, medianHeight)]));
  const sorted = [...glyphs].sort((a, b) => metrics.get(a).top - metrics.get(b).top || a.left - b.left);

  for (const glyph of sorted) {
    const glyphMetrics = metrics.get(glyph);
    let line = lines.find((candidate) => glyphMatchesLine(candidate, glyph, tolerance, glyphMetrics));
    if (!line) {
      line = {
        id: `line:${lines.length}`,
        glyphs: [],
        left: glyph.left,
        right: glyph.right,
        top: glyphMetrics.top,
        bottom: glyphMetrics.bottom,
        height: glyphMetrics.height,
        centerY: glyphMetrics.centerY,
        centerSum: 0
      };
      lines.push(line);
    }
    line.glyphs.push(glyph);
    line.centerSum += glyphMetrics.centerY;
    line.left = Math.min(line.left, glyph.left);
    line.right = Math.max(line.right, glyph.right);
    line.top = Math.min(line.top, glyphMetrics.top);
    line.bottom = Math.max(line.bottom, glyphMetrics.bottom);
    line.height = Math.max(line.height, glyphMetrics.height);
    line.centerY = line.centerSum / line.glyphs.length;
    glyph.line = line;
  }

  return lines.sort((a, b) => a.top - b.top);
}

function glyphMatchesLine(line, glyph, tolerance, metrics = null) {
  const glyphMetrics = metrics || glyphLineMetrics(glyph, line.height);
  const glyphHeight = glyphMetrics.height;
  const glyphCenterY = glyphMetrics.centerY;
  const minHeight = Math.max(1, Math.min(line.height, glyphHeight));
  const centerDistance = Math.abs(line.centerY - glyphCenterY);
  const strictDistance = Math.max(tolerance, minHeight * 0.45);
  if (centerDistance <= strictDistance) return true;

  const glyphTop = glyphMetrics.top;
  const glyphBottom = glyphMetrics.bottom;
  const overlap = Math.max(0, Math.min(line.bottom, glyphBottom) - Math.max(line.top, glyphTop));
  const overlapRatio = overlap / minHeight;
  const relaxedDistance = Math.max(tolerance * 1.6, minHeight * 0.78);
  return overlapRatio >= 0.45 && centerDistance <= relaxedDistance;
}

function glyphLineMetrics(glyph, medianHeight) {
  const rawTop = glyph.lineTop ?? glyph.top;
  const rawBottom = glyph.lineBottom ?? glyph.bottom;
  const rawHeight = Math.max(1, glyph.lineHeight ?? rawBottom - rawTop ?? glyph.height);
  const fallbackCenter = rawTop + rawHeight / 2;
  const centerY = Number.isFinite(glyph.lineCenterY) ? glyph.lineCenterY : Number.isFinite(glyph.centerY) ? glyph.centerY : fallbackCenter;
  const typicalHeight = Math.max(1, medianHeight || rawHeight);
  if (rawHeight <= typicalHeight * 1.8) {
    return {
      top: rawTop,
      bottom: rawBottom,
      height: rawHeight,
      centerY
    };
  }
  const height = typicalHeight;
  return {
    top: centerY - height / 2,
    bottom: centerY + height / 2,
    height,
    centerY
  };
}

function buildSegments(lines, options, pageWidth = 0) {
  const segments = [];
  for (const line of lines) {
    line.segments = [];
    const glyphs = [...line.glyphs].sort((a, b) => a.left - b.left || a.localIndex - b.localIndex);
    let current = [];
    let previous = null;
    for (const glyph of glyphs) {
      const gap = previous ? glyph.left - previous.right : 0;
      const gapLimit = Math.max(4, Math.min(28, line.height * options.segmentGapFactor));
      if (previous && gap > gapLimit && current.length > 0) {
        segments.push(segmentFromGlyphs(current, line, segments.length));
        current = [];
      }
      current.push(glyph);
      previous = glyph;
    }
    if (current.length > 0) segments.push(segmentFromGlyphs(current, line, segments.length));
  }
  markTableLikeLines(lines, pageWidth);
  return segments;
}

function markTableLikeLines(lines, pageWidth) {
  for (const line of lines) {
    line.isTableLike = false;
  }

  const candidateLines = lines.filter((line) => isTableCandidateLine(line, pageWidth));
  for (const line of candidateLines) {
    const alignedNeighbor = candidateLines.find((candidate) => {
      if (candidate === line) return false;
      const verticalGap = Math.max(0, Math.max(candidate.top, line.top) - Math.min(candidate.bottom, line.bottom));
      const height = Math.max(1, Math.min(candidate.height, line.height));
      return verticalGap <= height * 2.5 && alignedSegmentCount(line.segments, candidate.segments, pageWidth) >= 3;
    });
    if (alignedNeighbor || isStandaloneTableRow(line, pageWidth)) line.isTableLike = true;
  }

  const tableSeeds = lines.filter((line) => line.isTableLike);
  if (tableSeeds.length === 0) return;

  for (const line of lines) {
    if (line.isTableLike || line.segments.length < 2) continue;
    const height = Math.max(1, line.height || median(line.segments.map((segment) => segment.height), 8));
    const nearbyTableLine = tableSeeds.find((seed) => {
      const verticalGap = Math.max(0, Math.max(seed.top, line.top) - Math.min(seed.bottom, line.bottom));
      if (verticalGap > height * 2.4) return false;
      return alignedSegmentCount(line.segments, seed.segments, pageWidth) >= 2;
    });
    if (nearbyTableLine) line.isTableLike = true;
  }
}

function isTableCandidateLine(line, pageWidth) {
  if (!line || line.segments.length < 3) return false;
  const gaps = segmentGaps(line.segments);
  if (gaps.length < 2) return false;
  const largeGap = Math.max(line.height * 1.8, Math.min(36, (pageWidth || 400) * 0.035));
  const largeGapCount = gaps.filter((gap) => gap >= largeGap).length;
  if (largeGapCount < 2) return false;
  const coverage = lineTextCoverage(line);
  return coverage <= 0.72;
}

function isStandaloneTableRow(line, pageWidth) {
  if (!line || line.segments.length < 5) return false;
  const gaps = segmentGaps(line.segments);
  const span = Math.max(1, line.right - line.left);
  const averageGap = gaps.reduce((sum, gap) => sum + gap, 0) / Math.max(1, gaps.length);
  const minAverageGap = Math.max(line.height * 2.4, (pageWidth || span) * 0.055);
  return averageGap >= minAverageGap && lineTextCoverage(line) <= 0.58;
}

function segmentGaps(segments) {
  const ordered = [...(segments || [])].sort((a, b) => a.left - b.left);
  const gaps = [];
  for (let index = 1; index < ordered.length; index += 1) {
    gaps.push(Math.max(0, ordered[index].left - ordered[index - 1].right));
  }
  return gaps;
}

function lineTextCoverage(line) {
  const span = Math.max(1, line.right - line.left);
  const segmentWidth = (line.segments || []).reduce((sum, segment) => sum + Math.max(0, segment.width || 0), 0);
  return segmentWidth / span;
}

function alignedSegmentCount(aSegments, bSegments, pageWidth) {
  let count = 0;
  const tolerance = Math.max(5, Math.min(14, (pageWidth || 400) * 0.018));
  for (const a of aSegments) {
    const aligned = bSegments.some((b) => (
      Math.abs(a.left - b.left) <= tolerance ||
      Math.abs(a.right - b.right) <= tolerance
    ));
    if (aligned) count += 1;
  }
  return count;
}

function buildLayoutBlocks(segments, pageWidth = 0, pageHeight = 0) {
  const units = buildLayoutUnits(segments, pageWidth);
  const blocks = [];

  for (const unit of units.sort(readingUnitSort)) {
    let block = findAttachableBlock(blocks, unit);
    if (!block) {
      block = createLayoutBlock(unit, blocks.length);
      blocks.push(block);
    }
    addUnitToBlock(block, unit);
  }

  const sorted = blocks.sort(readingBlockSort);
  sorted.forEach((block, index) => {
    block.readingIndex = index;
    block.bounds = {
      left: block.left,
      top: block.top,
      right: block.right,
      bottom: block.bottom,
      width: block.right - block.left,
      height: block.bottom - block.top
    };
    block.pageWidth = pageWidth;
    block.pageHeight = pageHeight;
    for (const unit of block.units) {
      unit.block = block;
      for (const segment of unit.segments) {
        segment.block = block;
      }
      for (const glyph of unit.glyphs) {
        glyph.block = block;
      }
    }
  });
  return sorted;
}

function buildLayoutUnits(segments, pageWidth) {
  const units = [];
  const seenTableLines = new Set();
  const lineGroups = new Map();
  for (const segment of segments) {
    if (isMarginNumberSegment(segment, pageWidth)) continue;
    const line = segment.line;
    if (line?.isTableLike) {
      if (seenTableLines.has(line.id)) continue;
      seenTableLines.add(line.id);
      const rowSegments = (line.segments || []).filter((candidate) => !isMarginNumberSegment(candidate, pageWidth));
      units.push(unitFromSegments(`unit:${line.id}`, rowSegments, {
        line,
        type: 'table-row',
        laneIndex: -1
      }));
      continue;
    }
    const laneIndex = segment.laneIndex ?? -1;
    const key = `${line?.id || segment.id}:${laneIndex}`;
    const group = lineGroups.get(key);
    if (group) {
      group.segments.push(segment);
    } else {
      lineGroups.set(key, { line, laneIndex, segments: [segment] });
    }
  }
  for (const [key, group] of lineGroups) {
    const merged = unitFromSegments(`unit:${key}`, group.segments, {
      line: group.line,
      type: classifyTextUnitFromSegments(group.segments, pageWidth),
      laneIndex: group.laneIndex
    });
    units.push(merged);
  }
  return units.filter((unit) => unit.glyphs.length > 0);
}

function unitFromSegments(id, segments, extra) {
  let left = Infinity;
  let right = -Infinity;
  let top = Infinity;
  let bottom = -Infinity;
  const glyphs = [];
  for (const segment of segments) {
    left = Math.min(left, segment.left);
    right = Math.max(right, segment.right);
    top = Math.min(top, segment.top);
    bottom = Math.max(bottom, segment.bottom);
    glyphs.push(...segment.glyphs);
  }
  const width = right - left;
  const height = bottom - top;
  return {
    id,
    segments,
    glyphs,
    text: glyphs.slice().sort((a, b) => a.left - b.left || a.localIndex - b.localIndex).map((glyph) => glyph.char).join(''),
    left,
    right,
    top,
    bottom,
    width,
    height,
    centerX: left + width / 2,
    centerY: top + height / 2,
    ...extra
  };
}

function classifyTextUnit(segment, pageWidth) {
  const text = String(segment.text || '').trim();
  if (isSpanningSegment(segment, pageWidth) && text.length <= 120 && segment.height >= 14) return 'heading';
  if (/^([*\u2022\u25e6\u00b7-]|\d+[.)]|[A-Za-z][.)])\s*/.test(text)) return 'list';
  return 'paragraph';
}

function classifyTextUnitFromSegments(segments, pageWidth) {
  const merged = unitFromSegments('classification', segments, {});
  return classifyTextUnit(merged, pageWidth);
}

function findAttachableBlock(blocks, unit) {
  let best = null;
  for (let index = blocks.length - 1; index >= 0; index -= 1) {
    const block = blocks[index];
    if (!canAttachUnitToBlock(block, unit)) continue;
    const gap = unit.top - block.bottom;
    const score = Math.max(0, gap) * 10 + Math.abs(unit.left - block.left);
    if (!best || score < best.score) best = { block, score };
  }
  return best?.block || null;
}

function canAttachUnitToBlock(block, unit) {
  if (block.pageNumber !== unit.line?.glyphs?.[0]?.pageNumber) return false;
  if (block.type === 'table' || unit.type === 'table-row') {
    if (block.type !== 'table' || unit.type !== 'table-row') return false;
    const gap = unit.top - block.bottom;
    const height = Math.max(1, Math.min(block.medianLineHeight, unit.height));
    return gap >= -height * 0.55 && gap <= height * 2.2 && alignedSegmentCount(block.segments, unit.segments, block.pageWidth) >= 2;
  }
  if (block.type !== unit.type && !(block.type === 'paragraph' && unit.type === 'list')) return false;
  if (block.laneIndex !== unit.laneIndex) return false;
  const gap = unit.top - block.bottom;
  const height = Math.max(1, Math.min(block.medianLineHeight, unit.height));
  if (gap < -height * 0.45 || gap > height * 1.75) return false;
  if (isParagraphBoundary(block, unit, gap, height)) return false;
  const overlap = horizontalOverlapRatio(block, unit);
  const leftDelta = Math.abs(unit.left - block.left);
  const hangingDelta = Math.abs(unit.left - block.lastLeft);
  const indentLimit = Math.max(18, height * 4);
  return overlap >= 0.28 || leftDelta <= indentLimit || hangingDelta <= indentLimit;
}

function isParagraphBoundary(block, unit, gap, height) {
  if (block.type !== 'paragraph' || unit.type !== 'paragraph') return false;
  const first = block.units[0];
  const last = block.units.at(-1);
  if (!first || !last) return false;

  const blockWidth = Math.max(1, block.right - block.left);
  const expectedLineWidth = Math.max(blockWidth, first.width || 0, last.width || 0, unit.width || 0);
  const unitWidth = Math.max(1, unit.width);
  const rightGapLast = Math.max(0, expectedLineWidth - (last.right - first.left));
  const rightGapCurrent = Math.max(0, expectedLineWidth - (unit.right - first.left));
  const leftShiftFromFirst = unit.left - first.left;
  const leftShiftFromLast = unit.left - last.left;
  const indent = Math.max(10, Math.min(28, height * 1.8));

  const previousLineLooksComplete = last.width >= expectedLineWidth * 0.82 || rightGapLast <= Math.max(12, height * 2.2);
  const currentLooksIndented = leftShiftFromFirst >= indent && Math.abs(leftShiftFromLast) >= indent * 0.65;
  if (previousLineLooksComplete && currentLooksIndented) return true;

  const currentReturnsToLeft = Math.abs(unit.left - first.left) <= Math.max(6, height * 0.8);
  const blockHasContinuation = block.units.length >= 2 || last.width < blockWidth * 0.55;
  const previousIsShortLine = blockHasContinuation && last.width <= expectedLineWidth * 0.72 && rightGapLast >= Math.max(24, height * 3);
  if (previousIsShortLine && currentReturnsToLeft) return true;

  const currentIsHeadingLike = unit.glyphs.length <= 28 && unitWidth <= expectedLineWidth * 0.42 && rightGapCurrent >= expectedLineWidth * 0.4;
  if (gap > height * 0.45 && currentIsHeadingLike) return true;

  return false;
}

function createLayoutBlock(unit, index) {
  const pageNumber = unit.line?.glyphs?.[0]?.pageNumber ?? unit.glyphs?.[0]?.pageNumber ?? 0;
  const type = unit.type === 'table-row' ? 'table' : unit.type;
  return {
    id: `block:${index}`,
    pageNumber,
    type,
    laneIndex: unit.laneIndex,
    units: [],
    segments: [],
    lineIds: new Set(),
    left: Infinity,
    right: -Infinity,
    top: Infinity,
    bottom: -Infinity,
    lastLeft: unit.left,
    medianLineHeight: Math.max(1, unit.height),
    pageWidth: unit.glyphs?.[0]?.pageWidth || 0
  };
}

function addUnitToBlock(block, unit) {
  block.units.push(unit);
  block.segments.push(...unit.segments);
  block.lineIds.add(unit.line?.id);
  block.left = Math.min(block.left, unit.left);
  block.right = Math.max(block.right, unit.right);
  block.top = Math.min(block.top, unit.top);
  block.bottom = Math.max(block.bottom, unit.bottom);
  block.lastLeft = unit.left;
  block.medianLineHeight = median(block.units.map((entry) => entry.height), block.medianLineHeight);
}

function readingUnitSort(a, b) {
  const laneA = a.laneIndex ?? -1;
  const laneB = b.laneIndex ?? -1;
  if ((laneA < 0 || laneB < 0) && Math.abs(a.top - b.top) > 1.5) return a.top - b.top;
  if (laneA !== laneB) return laneA - laneB;
  if (Math.abs(a.top - b.top) > 1.5) return a.top - b.top;
  return a.left - b.left;
}

function readingBlockSort(a, b) {
  if ((a.laneIndex < 0 || b.laneIndex < 0) && Math.abs(a.top - b.top) > 1.5) return a.top - b.top;
  if (a.laneIndex !== b.laneIndex) return a.laneIndex - b.laneIndex;
  if (Math.abs(a.top - b.top) > 1.5) return a.top - b.top;
  return a.left - b.left;
}

function buildLanes(segments, pageWidth) {
  const bodySegments = segments
    .filter((segment) => !isMarginNumberSegment(segment, pageWidth))
    .filter((segment) => !segment.line?.isTableLike)
    .filter((segment) => {
      const minimumWidth = pageWidth > 500 ? Math.max(70, pageWidth * 0.14) : Math.max(30, segment.height * 3);
      return segment.glyphs.length >= 3 && segment.width >= minimumWidth;
    })
    .filter((segment) => !isSpanningSegment(segment, pageWidth));
  const clusters = [];

  for (const segment of bodySegments) {
    const tolerance = Math.max(32, Math.min(pageWidth || 0, 900) * 0.11);
    let cluster = clusters.find((candidate) => Math.abs(candidate.centerX - segment.centerX) <= tolerance);
    if (!cluster) {
      cluster = {
        left: segment.left,
        right: segment.right,
        centerX: segment.centerX,
        count: 0,
        area: 0,
        segments: []
      };
      clusters.push(cluster);
    }
    cluster.segments.push(segment);
    cluster.count += 1;
    cluster.area += segment.width * segment.height;
    cluster.left = Math.min(cluster.left, segment.left);
    cluster.right = Math.max(cluster.right, segment.right);
    cluster.centerX = ((cluster.centerX * (cluster.count - 1)) + segment.centerX) / cluster.count;
  }

  let lanes = clusters.filter((cluster) => cluster.count >= 2);
  if (lanes.length > 2) {
    lanes = [...lanes].sort((a, b) => b.area - a.area).slice(0, 2);
  }
  lanes = lanes.sort((a, b) => a.left - b.left);
  if (lanes.length === 0) {
    lanes = [{ left: 0, right: pageWidth || Infinity, centerX: (pageWidth || 0) / 2, count: segments.length, segments: [] }];
  }

  for (const segment of segments) {
    if (isMarginNumberSegment(segment, pageWidth)) continue;
    if (segment.line?.isTableLike) continue;
    if (isSpanningSegment(segment, pageWidth)) continue;
    let best = null;
    for (const lane of lanes) {
      const overlapBonus = horizontalOverlapRatio(lane, segment) * Math.max(40, (pageWidth || 300) * 0.08);
      const score = Math.abs(segment.centerX - lane.centerX) - overlapBonus;
      if (!best || score < best.score) best = { lane, score };
    }
    best?.lane.segments.push(segment);
  }

  return lanes.map((lane, index) => ({
    ...lane,
    index,
    segments: uniqueSegments(lane.segments)
  }));
}

function isSpanningSegment(segment, pageWidth) {
  return Boolean(pageWidth > 0 && segment.width >= pageWidth * 0.6);
}

function isMarginNumberSegment(segment, pageWidth) {
  if (!(pageWidth > 0) || !segment) return false;
  const text = String(segment.text || '').trim();
  if (!/^\d{1,5}$/.test(text)) return false;
  if (segment.width > Math.max(24, segment.height * 4)) return false;
  return segment.right < pageWidth * 0.12 || segment.left > pageWidth * 0.88;
}

function selectionLaneForGlyph(glyph) {
  if (!glyph || !Number.isFinite(glyph.laneIndex) || glyph.laneIndex < 0) return null;
  const width = Math.max(1, glyph.laneRight - glyph.laneLeft);
  const margin = Math.max(8, Math.min(width * 0.08, glyph.height * 2));
  return {
    pageNumber: glyph.pageNumber,
    laneIndex: glyph.laneIndex,
    left: glyph.laneLeft,
    right: glyph.laneRight,
    margin
  };
}

function lineKeyForGlyph(glyph) {
  if (!glyph?.line?.id) return '';
  return `${glyph.pageNumber}:${glyph.laneIndex ?? -1}:${glyph.line.id}`;
}

function laneLineKey(pageNumber, laneIndex) {
  return `${pageNumber}:${laneIndex ?? -1}`;
}

function glyphMatchesLane(glyph, lane) {
  if (!lane || !glyph || glyph.pageNumber !== lane.pageNumber) return true;
  if (glyph.laneIndex !== lane.laneIndex) return false;
  return glyph.centerX >= lane.left - lane.margin && glyph.centerX <= lane.right + lane.margin;
}

function glyphBelongsToLane(glyph, lane) {
  if (!lane || !glyph || glyph.pageNumber !== lane.pageNumber) return true;
  return glyph.laneIndex === lane.laneIndex;
}

function lanesFromGlyphs(glyphs) {
  const lanes = new Map();
  for (const glyph of glyphs) {
    if (!Number.isFinite(glyph.laneIndex) || glyph.laneIndex < 0) continue;
    const current = lanes.get(glyph.laneIndex) || {
      pageNumber: glyph.pageNumber,
      laneIndex: glyph.laneIndex,
      left: glyph.laneLeft,
      right: glyph.laneRight
    };
    current.left = Math.min(current.left, glyph.laneLeft);
    current.right = Math.max(current.right, glyph.laneRight);
    lanes.set(glyph.laneIndex, current);
  }
  return Array.from(lanes.values())
    .map((lane) => ({
      ...lane,
      margin: Math.max(10, Math.min(24, (lane.right - lane.left) * 0.06))
    }))
    .sort((a, b) => a.left - b.left);
}

function laneForPoint(page, x) {
  const lanes = page?.lanes || [];
  let best = null;
  for (const lane of lanes) {
    if (x < lane.left - lane.margin || x > lane.right + lane.margin) continue;
    const score = Math.abs(x - (lane.left + lane.right) / 2);
    if (!best || score < best.score) best = { lane, score };
  }
  return best?.lane || null;
}

function boundaryForLineEdge(glyphs, x, y) {
  if (!glyphs || glyphs.length === 0) return null;
  const ordered = [...glyphs].sort((a, b) => a.left - b.left || a.localIndex - b.localIndex);
  if (!pointInLineCoreBand(ordered, y)) return null;
  const first = ordered[0];
  const last = ordered.at(-1);
  const left = Math.min(...ordered.map((glyph) => (glyph.hitRect || glyph.rect).left));
  const right = Math.max(...ordered.map((glyph) => (glyph.hitRect || glyph.rect).right));
  const height = Math.max(1, ...ordered.map((glyph) => (glyph.hitRect || glyph.rect).height));
  if (x >= left && x <= right) return null;
  const outsidePadding = Math.max(4, Math.min(24, height * 1.6));
  if (x < left && x >= left - outsidePadding) return { glyph: first, boundary: first.order };
  if (x > right && x <= right + outsidePadding) return { glyph: last, boundary: last.order + 1 };
  return null;
}

function boundaryForBestLineEdge(glyphs, x, y, containingGlyphs = []) {
  if (!glyphs || glyphs.length === 0) return null;
  let best = null;
  for (const line of lineEntriesFromGlyphs(glyphs)) {
    if (!canUseLineEdgeBoundary(line.glyphs, containingGlyphs)) continue;
    const edge = boundaryForLineEdge(line.glyphs, x, y);
    if (!edge) continue;
    const score = lineEdgeScore(line, x, y);
    if (!best || score < best.score) best = { edge, score };
  }
  return best?.edge || null;
}

function canUseLineEdgeBoundary(lineGlyphs, containingGlyphs) {
  if (!lineGlyphs || lineGlyphs.length === 0) return false;
  if (!containingGlyphs || containingGlyphs.length === 0) return true;
  const lineKeys = new Set(lineGlyphs.map((glyph) => lineKeyForGlyph(glyph)));
  return containingGlyphs.every((glyph) => lineKeys.has(lineKeyForGlyph(glyph)));
}

function lineEntriesFromGlyphs(glyphs) {
  const lines = new Map();
  for (const glyph of glyphs) {
    const key = lineKeyForGlyph(glyph) || `${glyph.pageNumber}:${glyph.laneIndex}:${glyph.localIndex}`;
    let line = lines.get(key);
    const rect = glyph.hitRect || glyph.rect;
    if (!line) {
      line = {
        key,
        glyphs: [],
        left: rect.left,
        right: rect.right,
        top: rect.top,
        bottom: rect.bottom
      };
      lines.set(key, line);
    }
    line.glyphs.push(glyph);
    line.left = Math.min(line.left, rect.left);
    line.right = Math.max(line.right, rect.right);
    line.top = Math.min(line.top, rect.top);
    line.bottom = Math.max(line.bottom, rect.bottom);
  }
  return Array.from(lines.values());
}

function lineEdgeScore(line, x, y) {
  const verticalDistance = y < line.top ? line.top - y : y > line.bottom ? y - line.bottom : 0;
  const horizontalDistance = x < line.left ? line.left - x : x > line.right ? x - line.right : 0;
  const centerY = (line.top + line.bottom) / 2;
  return verticalDistance * 1000 + horizontalDistance * 8 + Math.abs(y - centerY);
}

function pointInLineCoreBand(ordered, y) {
  if (!ordered || ordered.length === 0 || !Number.isFinite(y)) return false;
  let top = Infinity;
  let bottom = -Infinity;
  let height = 0;
  for (const glyph of ordered) {
    const rect = glyph.hitRect || glyph.rect;
    top = Math.min(top, rect.top);
    bottom = Math.max(bottom, rect.bottom);
    height = Math.max(height, rect.height || glyph.height || 0);
  }
  if (!Number.isFinite(top) || !Number.isFinite(bottom)) return false;
  const padding = Math.max(1, Math.min(4, height * 0.2));
  return y >= top - padding && y <= bottom + padding;
}

function boundaryForOrderedLine(ordered, x) {
  if (!ordered || ordered.length === 0 || !Number.isFinite(x)) return null;
  const first = ordered[0];
  const last = ordered.at(-1);
  let left = Infinity;
  let right = -Infinity;
  for (const glyph of ordered) {
    const rect = glyph.hitRect || glyph.rect;
    left = Math.min(left, rect.left);
    right = Math.max(right, rect.right);
  }
  const edgePadding = Math.max(1, Math.min(first.height, last.height) * 0.35);
  if (x <= left + edgePadding) return first.order;
  if (x >= right - edgePadding) return last.order + 1;
  let best = null;
  for (const glyph of ordered) {
    const rect = glyph.hitRect || glyph.rect;
    const center = rect.left + rect.width / 2;
    const dx = x < rect.left ? rect.left - x : x > rect.right ? x - rect.right : 0;
    const score = dx * 100 + Math.abs(x - center);
    if (!best || score < best.score) {
      best = {
        glyph,
        score,
        boundary: x <= center ? glyph.order : glyph.order + 1
      };
    }
  }
  return best?.boundary ?? null;
}

function glyphsForClosestLine(glyphs, y) {
  const lines = new Map();
  for (const glyph of glyphs) {
    const key = lineKeyForGlyph(glyph) || `${glyph.pageNumber}:${glyph.laneIndex}:${glyph.localIndex}`;
    const current = lines.get(key) || {
      glyphs: [],
      top: glyph.lineTop ?? glyph.top,
      bottom: glyph.lineBottom ?? glyph.bottom,
      centerY: lineCenterY(glyph.line) || glyph.lineCenterY || glyph.centerY
    };
    current.glyphs.push(glyph);
    current.top = Math.min(current.top, glyph.lineTop ?? glyph.top);
    current.bottom = Math.max(current.bottom, glyph.lineBottom ?? glyph.bottom);
    current.centerY = lineCenterY(current);
    lines.set(key, current);
  }
  let best = null;
  for (const line of lines.values()) {
    const score = lineScoreForY(line, y);
    if (!best || score < best.score) best = { line, score };
  }
  return best?.line?.glyphs || [];
}

function lineScoreForY(line, y) {
  const centerY = lineCenterY(line);
  const halfHeight = Math.max(1, (line.bottom - line.top) / 2);
  const distance = Math.abs(y - centerY);
  if (y >= line.top && y <= line.bottom) return distance;
  return halfHeight + distance * 2;
}

function lineCenterY(line) {
  const center = Number(line?.centerY);
  if (Number.isFinite(center)) return center;
  return (Number(line?.top) + Number(line?.bottom)) / 2;
}

function segmentFromGlyphs(glyphs, line, index) {
  let left = Infinity;
  let right = -Infinity;
  const text = [];
  for (const glyph of glyphs) {
    left = Math.min(left, glyph.rect.left);
    right = Math.max(right, glyph.rect.right);
    text.push(glyph.char);
  }
  const top = line.top;
  const bottom = line.bottom;
  const segment = {
    id: `segment:${index}`,
    line,
    glyphs,
    left,
    right,
    top,
    bottom,
    width: right - left,
    height: bottom - top,
    centerX: left + (right - left) / 2,
    text: text.join('')
  };
  line.segments?.push(segment);
  for (const glyph of glyphs) glyph.segment = segment;
  return segment;
}

function buildSelectedText(glyphs, pageMap = null) {
  const indexedText = buildSelectedTextFromCharRanges(glyphs, pageMap);
  if (indexedText) return indexedText;
  const parts = [];
  let previous = null;
  for (const glyph of glyphs) {
    if (previous) {
      const lineChanged = !glyphsAreOnSameTextLine(previous, glyph);
      if (lineChanged) {
        const separator = glyphLineSeparator(previous, glyph);
        if (separator === 'dehyphenate') {
          const last = parts.at(-1) || '';
          parts[parts.length - 1] = last.replace(/[-\u2010-\u2015]\s*$/, '');
        } else {
          parts.push(separator);
        }
      } else if (shouldInsertSpace(previous, glyph)) {
        parts.push(' ');
      }
    }
    parts.push(glyph.char);
    previous = glyph;
  }
  return parts.join('').replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
}

function glyphLineSeparator(previous, glyph) {
  if (!previous || !glyph) return '\n';
  if (previous.pageNumber !== glyph.pageNumber) return '\n';
  if (previous.block?.id && glyph.block?.id && previous.block.id !== glyph.block.id) return '\n';
  if (previous.block?.type === 'table' || glyph.block?.type === 'table') return '\n';
  const previousText = String(previous.char || '');
  const currentText = String(glyph.char || '');
  if (/[-\u2010-\u2015]$/.test(previousText) && /^[a-z]/.test(currentText)) return 'dehyphenate';
  return '\n';
}

function glyphsAreOnSameTextLine(previous, glyph) {
  if (!previous || !glyph) return false;
  if (previous.pageNumber !== glyph.pageNumber || previous.line?.id !== glyph.line?.id) return false;
  if (previous.laneIndex === glyph.laneIndex) return true;
  const gap = glyph.left - previous.right;
  const maxInlineGap = Math.max(12, Math.min(previous.height, glyph.height) * 2);
  return gap >= -1 && gap <= maxInlineGap;
}

function completeInteriorLineGlyphs(glyphs, lineGlyphs) {
  if (!lineGlyphs || glyphs.length === 0) return glyphs;
  const ordered = [...glyphs].sort((a, b) => a.order - b.order);
  const lineOrder = [];
  const seenLines = new Set();
  for (const glyph of ordered) {
    const key = lineKeyForGlyph(glyph);
    if (!key || seenLines.has(key)) continue;
    seenLines.add(key);
    lineOrder.push(key);
  }
  if (lineOrder.length <= 2) return ordered;

  const byOrder = new Map(ordered.map((glyph) => [glyph.order, glyph]));
  for (const key of lineOrder.slice(1, -1)) {
    const lineEntry = lineGlyphs.get(key);
    for (const glyph of lineEntry?.glyphs || []) {
      byOrder.set(glyph.order, glyph);
    }
  }
  return Array.from(byOrder.values()).sort((a, b) => a.order - b.order);
}

function buildSelectedTextFromCharRanges(glyphs, pageMap) {
  if (!pageMap || glyphs.length === 0) return '';
  const ranges = [];
  let previousPage = null;
  let previousLineKey = '';
  let rangeStart = null;
  let rangeEnd = null;
  let rangeFirstGlyph = null;
  let rangeLastGlyph = null;

  const flushRange = () => {
    if (rangeStart === null || rangeEnd === null || !previousPage || !rangeFirstGlyph || !rangeLastGlyph) return;
    const text = String(previousPage.text || '');
    if (!text) return;
    const slice = text.slice(rangeStart, rangeEnd);
    if (!slice) return;
    ranges.push({
      text: slice,
      pageNumber: rangeFirstGlyph.pageNumber,
      laneIndex: rangeFirstGlyph.laneIndex ?? -1,
      blockId: rangeFirstGlyph.block?.id ?? '',
      blockType: rangeFirstGlyph.block?.type ?? '',
      lineTop: rangeFirstGlyph.line?.top ?? rangeFirstGlyph.lineTop ?? rangeFirstGlyph.top,
      lineBottom: rangeFirstGlyph.line?.bottom ?? rangeFirstGlyph.lineBottom ?? rangeFirstGlyph.bottom,
      lineHeight: rangeFirstGlyph.line?.height ?? rangeFirstGlyph.lineHeight ?? rangeFirstGlyph.height,
      left: rangeFirstGlyph.left,
      right: rangeLastGlyph.right
    });
  };

  for (const glyph of glyphs) {
    if (!Number.isFinite(glyph.charIndex)) return '';
    const page = pageMap.get(glyph.pageNumber);
    if (!page?.text) return '';
    const lineKey = `${glyph.pageNumber}:${glyph.laneIndex ?? -1}:${glyph.line?.id ?? ''}`;
    const start = glyph.charIndex;
    const end = glyph.charIndex + Array.from(glyph.char || '').length;
    if (previousPage !== page || previousLineKey !== lineKey || rangeEnd === null || start > rangeEnd + 12 || start < rangeStart) {
      flushRange();
      previousPage = page;
      previousLineKey = lineKey;
      rangeStart = start;
      rangeEnd = end;
      rangeFirstGlyph = glyph;
      rangeLastGlyph = glyph;
    } else {
      rangeEnd = Math.max(rangeEnd, end);
      rangeLastGlyph = glyph;
    }
  }
  flushRange();
  return joinTextRanges(ranges);
}

function joinTextRanges(ranges) {
  const parts = [];
  let previous = null;
  for (const range of ranges) {
    const text = String(range.text || '').trim();
    if (!text) continue;
    if (!previous) {
      parts.push(text);
    } else {
      const separator = textRangeSeparator(previous, range);
      if (separator === 'dehyphenate') {
        const last = parts.at(-1) || '';
        parts[parts.length - 1] = last.replace(/[-\u2010-\u2015]\s*$/, '');
      } else {
        parts.push(separator);
      }
      parts.push(text);
    }
    previous = range;
  }
  return parts.join('').replace(/[ \t]+\n/g, '\n').replace(/[ \t]{2,}/g, ' ').replace(/\n{3,}/g, '\n\n').trim();
}

function textRangeSeparator(previous, current) {
  if (previous.pageNumber !== current.pageNumber || previous.laneIndex !== current.laneIndex) return '\n';
  if (previous.blockId && current.blockId && previous.blockId !== current.blockId) return '\n';
  const previousText = String(previous.text || '').trimEnd();
  const currentText = String(current.text || '').trimStart();
  if (/[-\u2010-\u2015]$/.test(previousText) && /^[a-z]/.test(currentText)) return 'dehyphenate';
  const height = Math.max(1, previous.lineHeight || current.lineHeight || 1);
  const verticalGap = (current.lineTop ?? 0) - (previous.lineBottom ?? 0);
  if (verticalGap > height * 1.25) return '\n';
  return ' ';
}

function buildSelectionRects(glyphs, lineGlyphs = null) {
  const rectGlyphs = completeInteriorLineGlyphs(glyphs, lineGlyphs);
  const merged = [];
  for (const glyph of rectGlyphs) {
    const current = {
      pageNumber: glyph.pageNumber,
      lineId: glyph.line?.id,
      laneIndex: glyph.laneIndex,
      left: glyph.rect.left,
      top: glyph.line?.top ?? glyph.rect.top,
      right: glyph.rect.right,
      bottom: glyph.line?.bottom ?? glyph.rect.bottom,
      width: glyph.rect.width,
      height: glyph.line?.height ?? glyph.rect.height
    };
    const previous = merged.at(-1);
    const sameLine = previous && previous.pageNumber === current.pageNumber && previous.lineId === current.lineId && previous.laneIndex === current.laneIndex;
    const gap = sameLine ? current.left - previous.right : Infinity;
    if (sameLine && gap <= Math.max(3, Math.min(previous.height, current.height) * 0.9)) {
      previous.right = Math.max(previous.right, current.right);
      previous.top = Math.min(previous.top, current.top);
      previous.bottom = Math.max(previous.bottom, current.bottom);
      previous.width = previous.right - previous.left;
      previous.height = previous.bottom - previous.top;
    } else {
      merged.push(current);
    }
  }
  return merged.map(({ lineId, laneIndex, right, bottom, ...rect }) => rect);
}

function shouldInsertSpace(previous, glyph) {
  if (previous.isSpace || glyph.isSpace) return false;
  const left = previous.char.at(-1) || '';
  const right = glyph.char[0] || '';
  if (/\s/.test(left) || /\s/.test(right)) return false;
  if ('([{/"\'-'.includes(left)) return false;
  if ('.,;:!?)]}%"\'/-'.includes(right)) return false;
  const gap = glyph.left - previous.right;
  const threshold = Math.max(2.5, Math.min(previous.height, glyph.height) * 0.3);
  return gap > threshold;
}

function expandRect(rect, padding) {
  return {
    left: rect.left - padding,
    top: rect.top - padding,
    right: rect.right + padding,
    bottom: rect.bottom + padding,
    width: rect.width + padding * 2,
    height: rect.height + padding * 2
  };
}

function pointInsideRect(x, y, rect) {
  return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
}

function verticalOverlapRatio(a, b) {
  const overlap = Math.max(0, Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top));
  return overlap / Math.max(1, Math.min(a.height, b.height));
}

function horizontalOverlapRatio(a, b) {
  const overlap = Math.max(0, Math.min(a.right, b.right) - Math.max(a.left, b.left));
  return overlap / Math.max(1, Math.min(a.right - a.left, b.right - b.left));
}

function uniqueSegments(segments) {
  const seen = new Set();
  const unique = [];
  for (const segment of segments) {
    if (seen.has(segment.id)) continue;
    seen.add(segment.id);
    unique.push(segment);
  }
  return unique;
}
