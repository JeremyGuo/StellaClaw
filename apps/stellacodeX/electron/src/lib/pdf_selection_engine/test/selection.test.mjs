import assert from 'node:assert/strict';
import test from 'node:test';
import { PdfSelectionIndex, glyphPageFromEmbedPdf } from '../src/index.js';

test('selects glyphs using glyph boundaries, not run width guesses', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 200,
    glyphs: glyphsForText('Wide', 10, 20, [14, 4, 9, 10])
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const anchor = index.hitTest({ pageNumber: 1, x: 9, y: 25 });
  const focus = index.hitTest({ pageNumber: 1, x: 37, y: 25 });
  const selection = index.selectBetween(anchor, focus);
  assert.equal(selection.selectedText, 'Wid');
  assert.equal(selection.glyphCount, 3);
});

test('orders two-column text by lane before vertical continuation', () => {
  const page = {
    pageNumber: 1,
    width: 420,
    height: 240,
    glyphs: [
      ...glyphsForText('LEFTONE', 20, 20),
      ...glyphsForText('LEFTTWO', 20, 40),
      ...glyphsForText('LEFTTHR', 20, 60),
      ...glyphsForText('RGHTONE', 220, 20),
      ...glyphsForText('RGHTTWO', 220, 40),
      ...glyphsForText('RGHTTHR', 220, 60)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const start = index.hitTest({ pageNumber: 1, x: 18, y: 25 });
  const end = index.hitTest({ pageNumber: 1, x: 270, y: 45 });
  const selection = index.selectBetween(start, end, { sameLaneSelection: false });
  assert.match(selection.selectedText, /^LEFTONE\nLEFTTWO\nLEFTTHR\nRGHTONE/);
});

test('same-lane drag does not spill into the opposite column', () => {
  const page = {
    pageNumber: 1,
    width: 420,
    height: 240,
    glyphs: [
      ...glyphsForText('LEFTONE', 20, 20),
      ...glyphsForText('LEFTTWO', 20, 40),
      ...glyphsForText('LEFTTHR', 20, 60),
      ...glyphsForText('RGHTONE', 220, 20),
      ...glyphsForText('RGHTTWO', 220, 40),
      ...glyphsForText('RGHTTHR', 220, 60)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const startPoint = { pageNumber: 1, x: 218, y: 25 };
  const endPoint = { pageNumber: 1, x: 70, y: 65 };
  const start = index.hitTest(startPoint);
  const end = index.hitTest(endPoint);
  const selection = index.selectBetween(start, end, { focusPoint: endPoint });
  assert.match(selection.selectedText, /^RGHTONE\nRGHTTWO/);
  assert.doesNotMatch(selection.selectedText, /LEFT/);
});

test('same-lane range selection does not drop glyphs at lane edge', () => {
  const page = {
    pageNumber: 1,
    width: 420,
    height: 240,
    glyphs: [
      ...glyphsForText('LEFTONE', 20, 20),
      ...glyphsForText('LEFTTWO', 20, 40),
      ...glyphsForText('LEFTTHR', 20, 60),
      ...glyphsForText('RGHTONE', 220, 20),
      ...glyphsForText('RGHTTWO', 220, 40),
      ...glyphsForText('RGHTTHR', 220, 60)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const start = index.hitTest({ pageNumber: 1, x: 218, y: 25 });
  const end = index.hitTest({ pageNumber: 1, x: 282, y: 25 });
  start.glyph.laneRight = start.glyph.left + 12;
  const selection = index.selectBetween(start, end);
  assert.equal(selection.selectedText, 'RGHTONE');
});

test('multi-line selection completes interior line prefixes', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 180,
    glyphs: [
      ...glyphsForText('AAAA', 20, 20),
      ...glyphsForText('SBBB', 20, 40),
      ...glyphsForText('CCCC', 20, 60)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const interiorS = index.glyphs.find((glyph) => glyph.char === 'S');
  interiorS.laneIndex = -1;

  const start = index.hitTest({ pageNumber: 1, x: 18, y: 25 });
  const end = index.hitTest({ pageNumber: 1, x: 60, y: 65 });
  const selection = index.selectBetween(start, end);
  assert.match(selection.selectedText, /AAAA\nSBBB\nCCCC/);
  assert.ok(selection.rects.some((rect) => rect.left === 20 && rect.top >= 39 && rect.top <= 42));
});

test('downward drag near the next line top stays on previous line', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 120,
    glyphs: [
      ...glyphsForText('AAA', 20, 20),
      ...glyphsForText('BBB', 20, 34)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const startPoint = { pageNumber: 1, x: 18, y: 25 };
  const endPoint = { pageNumber: 1, x: 28, y: 37 };
  const start = index.hitTest(startPoint);
  const end = index.hitTest(endPoint);
  const selection = index.selectBetween(start, end, { focusPoint: endPoint });
  assert.equal(selection.selectedText, 'AAA');
});

test('overlapping loose glyph boxes do not merge adjacent visual lines', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 120,
    glyphs: [
      ...glyphsForTextWithRects('AAA', 20, 20, 20, 21, 8),
      ...glyphsForTextWithRects('BBB', 20, 34, 20, 35, 8)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  assert.equal(index.lineGlyphs.size, 2);

  const startPoint = { pageNumber: 1, x: 18, y: 25 };
  const endPoint = { pageNumber: 1, x: 44, y: 35 };
  const start = index.hitTest(startPoint);
  const end = index.hitTest(endPoint);
  const selection = index.selectBetween(start, end, { focusPoint: endPoint });
  assert.equal(selection.selectedText, 'AAA');
});

test('line clustering keeps adjacent lines separate when loose boxes overlap', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 120,
    glyphs: [
      ...glyphsForTextWithRects('AAA', 20, 20, 20, 21, 8),
      ...glyphsForTextWithRects('BBB', 20, 30, 20, 31, 8)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  assert.equal(index.lineGlyphs.size, 2);
});

test('varied tight boxes on one visual line stay grouped', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 120,
    glyphs: [
      glyphWithRects('A', 20, 20, 12, 21, 7),
      glyphWithRects('g', 28, 20, 12, 25, 7),
      glyphWithRects('e', 36, 20, 12, 24, 6),
      glyphWithRects('n', 44, 20, 12, 23, 6),
      ...glyphsForTextWithRects('BBB', 20, 36, 20, 37, 8)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  assert.equal(index.lineGlyphs.size, 2);

  const start = index.hitTest({ pageNumber: 1, x: 18, y: 25 });
  const end = index.hitTest({ pageNumber: 1, x: 52, y: 25 });
  const selection = index.selectBetween(start, end);
  assert.equal(selection.selectedText, 'Agen');
});

test('line clustering uses loose rects so shifted tight boxes stay on visual line', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 140,
    glyphs: [
      glyphWithRects('S', 20, 20, 10, 32, 8),
      ...glyphsForTextWithRects('imFactory', 28, 20, 10, 21, 8),
      ...glyphsForTextWithRects('NEXT', 20, 36, 10, 37, 8)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const firstLine = Array.from(index.lineGlyphs.values()).find((line) => line.glyphs.some((glyph) => glyph.char === 'S'));
  assert.equal(firstLine.glyphs.map((glyph) => glyph.char).join(''), 'SimFactory');
  assert.equal(index.lineGlyphs.size, 2);
});

test('hitTest prefers glyph under pointer over adjacent line edge snap', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 140,
    glyphs: [
      glyphWithRects('U', 100, 20, 12, 24, 12),
      glyphWithRects('L', 100, 34, 10, 35, 8)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const upperLine = Array.from(index.lineGlyphs.values()).find((line) => line.glyphs[0]?.char === 'U');
  const lowerLine = Array.from(index.lineGlyphs.values()).find((line) => line.glyphs[0]?.char === 'L');
  upperLine.centerY = 20;
  lowerLine.centerY = 33.5;

  const hit = index.hitTest({ pageNumber: 1, x: 100.5, y: 33.5 });
  assert.equal(hit.glyph.char, 'U');
  assert.equal(hit.boundary, hit.glyph.order);
});

test('tall symbol boxes do not inflate line or selection rect height', () => {
  const page = {
    pageNumber: 1,
    width: 320,
    height: 160,
    glyphs: [
      ...glyphsForText('rate', 20, 20),
      glyphWithRects('×', 52, 0, 50, 0, 50),
      ...glyphsForText('speed', 60, 20),
      ...glyphsForText('NEXT', 20, 40)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const firstLine = Array.from(index.lineGlyphs.values()).find((line) => line.glyphs.some((glyph) => glyph.char === '×'));
  assert.ok(firstLine.height <= 12);
  assert.equal(firstLine.glyphs.map((glyph) => glyph.char).join(''), 'rate×speed');

  const start = index.hitTest({ pageNumber: 1, x: 18, y: 25 });
  const end = index.hitTest({ pageNumber: 1, x: 98, y: 25 });
  const selection = index.selectBetween(start, end);
  assert.equal(selection.selectedText, 'rate×speed');
  assert.ok(selection.rects.every((rect) => rect.height <= 12));
});

test('table-like rows select across multiple cell segments', () => {
  const page = {
    pageNumber: 1,
    width: 360,
    height: 160,
    glyphs: [
      ...glyphsForText('Cat', 20, 20),
      ...glyphsForText('Func', 90, 20),
      ...glyphsForText('Scope', 160, 20),
      ...glyphsForText('Share', 250, 20),
      ...glyphsForText('Total', 310, 20)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const row = Array.from(index.lineGlyphs.values()).find((line) => line.glyphs.some((glyph) => glyph.char === 'C'));
  assert.equal(row.laneIndex, -1);

  const start = index.hitTest({ pageNumber: 1, x: 18, y: 25 });
  const end = index.hitTest({ pageNumber: 1, x: 348, y: 25 });
  const selection = index.selectBetween(start, end, { focusPoint: { pageNumber: 1, x: 348, y: 25 } });
  assert.equal(selection.selectedText, 'Cat Func Scope Share Total');
});

test('table-like multi-row selections keep row-major order', () => {
  const page = {
    pageNumber: 1,
    width: 360,
    height: 180,
    glyphs: [
      ...glyphsForText('A1', 20, 20),
      ...glyphsForText('B1', 90, 20),
      ...glyphsForText('C1', 160, 20),
      ...glyphsForText('D1', 250, 20),
      ...glyphsForText('E1', 310, 20),
      ...glyphsForText('A2', 20, 40),
      ...glyphsForText('B2', 90, 40),
      ...glyphsForText('C2', 160, 40),
      ...glyphsForText('D2', 250, 40),
      ...glyphsForText('E2', 310, 40)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const start = index.hitTest({ pageNumber: 1, x: 18, y: 25 });
  const endPoint = { pageNumber: 1, x: 330, y: 45 };
  const end = index.hitTest(endPoint);
  const selection = index.selectBetween(start, end, { focusPoint: endPoint });
  assert.equal(selection.selectedText, 'A1 B1 C1 D1 E1\nA2 B2 C2 D2 E2');
});

test('downward drag into the next line body still crosses line', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 120,
    glyphs: [
      ...glyphsForText('AAA', 20, 20),
      ...glyphsForText('BBB', 20, 34)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const startPoint = { pageNumber: 1, x: 18, y: 25 };
  const endPoint = { pageNumber: 1, x: 36, y: 40 };
  const start = index.hitTest(startPoint);
  const end = index.hitTest(endPoint);
  const selection = index.selectBetween(start, end, { focusPoint: endPoint });
  assert.equal(selection.selectedText, 'AAA\nBB');
});

test('upward drag near the previous line bottom stays on current line', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 120,
    glyphs: [
      ...glyphsForText('AAA', 20, 20),
      ...glyphsForText('BBB', 20, 34)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const startPoint = { pageNumber: 1, x: 44, y: 39 };
  const endPoint = { pageNumber: 1, x: 28, y: 27 };
  const start = index.hitTest(startPoint);
  const end = index.hitTest(endPoint);
  const selection = index.selectBetween(start, end, { focusPoint: endPoint });
  assert.equal(selection.selectedText, 'BBB');
});

test('page order is stable even when pages arrive out of render order', () => {
  const index = PdfSelectionIndex.fromPages([
    { pageNumber: 2, width: 300, height: 120, glyphs: glyphsForText('TWO', 20, 20) },
    { pageNumber: 1, width: 300, height: 120, glyphs: glyphsForText('ONE', 20, 20) }
  ]);
  assert.equal(index.glyphs.slice(0, 3).map((glyph) => glyph.char).join(''), 'ONE');
});

test('area selection selects only glyph centers inside rectangle', () => {
  const page = {
    pageNumber: 1,
    width: 300,
    height: 160,
    glyphs: [
      ...glyphsForText('left', 20, 20),
      ...glyphsForText('right', 160, 20)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const selection = index.selectArea({ pageNumber: 1, left: 0, top: 0, width: 90, height: 60 });
  assert.equal(selection.selectedText, 'left');
});

test('selected text preserves spaces from source text runs', () => {
  const page = glyphPageFromEmbedPdf({
    page: {
      pageNumber: 1,
      width: 500,
      height: 120
    },
    pageIndex: 0,
    textRuns: {
      runs: [
        { charIndex: 0, text: 'simulators from structured protocol specifications. The goal' }
      ]
    },
    geometry: {
      runs: [
        {
          charStart: 0,
          glyphs: glyphGeometryForText('simulators from structured protocol specifications. The goal')
        }
      ]
    }
  });
  const index = PdfSelectionIndex.fromPages([page]);
  const start = index.hitTest({ pageNumber: 1, x: 9, y: 15 });
  const end = index.hitTest({ pageNumber: 1, x: 480, y: 15 });
  const selection = index.selectBetween(start, end);
  assert.equal(selection.selectedText, 'simulators from structured protocol specifications. The goal');
});

test('selected text reflows visual line wraps inside a paragraph', () => {
  const lineOne = 'This approach raises three design challenges. First,';
  const lineTwo = 'it must refine the generated simulator against a reference.';
  const source = `${lineOne} ${lineTwo}`;
  const page = glyphPageFromEmbedPdf({
    page: {
      pageNumber: 1,
      width: 520,
      height: 140
    },
    pageIndex: 0,
    textRuns: {
      runs: [
        { charIndex: 0, text: source }
      ]
    },
    geometry: {
      runs: [
        {
          charStart: 0,
          glyphs: glyphGeometryForText(lineOne, 10, 10)
        },
        {
          charStart: lineOne.length + 1,
          glyphs: glyphGeometryForText(lineTwo, 10, 24)
        }
      ]
    }
  });
  const index = PdfSelectionIndex.fromPages([page]);
  const start = index.hitTest({ pageNumber: 1, x: 9, y: 15 });
  const end = index.hitTest({ pageNumber: 1, x: 430, y: 29 });
  const selection = index.selectBetween(start, end);
  assert.equal(selection.selectedText, source);
});

test('layout blocks keep adjacent paragraphs separate without assuming paper columns', () => {
  const lineOne = 'First paragraph wraps here';
  const lineTwo = 'and continues on this line.';
  const lineThree = 'Second paragraph starts after a gap.';
  const page = {
    pageNumber: 1,
    width: 520,
    height: 180,
    glyphs: [
      ...glyphsForText(lineOne, 20, 20),
      ...glyphsForText(lineTwo, 20, 34),
      ...glyphsForText(lineThree, 20, 68)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const blocks = index.pageMap.get(1).blocks.filter((block) => block.type === 'paragraph');
  assert.equal(blocks.length, 2);
  assert.equal(blocks[0].units.length, 2);
  assert.equal(blocks[1].units.length, 1);
});

test('layout blocks do not merge side-by-side regions into one paragraph', () => {
  const page = {
    pageNumber: 1,
    width: 520,
    height: 180,
    glyphs: [
      ...glyphsForText('Left block first', 20, 20),
      ...glyphsForText('Left block second', 20, 34),
      ...glyphsForText('Right note first', 300, 20),
      ...glyphsForText('Right note second', 300, 34)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const blocks = index.pageMap.get(1).blocks.filter((block) => block.type === 'paragraph');
  assert.equal(blocks.length, 2);
  assert.ok(blocks.every((block) => block.units.length === 2));
});

test('table blocks include aligned two-segment continuation rows', () => {
  const page = {
    pageNumber: 1,
    width: 420,
    height: 180,
    glyphs: [
      ...glyphsForText('Category', 20, 20),
      ...glyphsForText('Function', 120, 20),
      ...glyphsForText('Scope', 220, 20),
      ...glyphsForText('Share', 340, 20),
      ...glyphsForText('Framework', 20, 38),
      ...glyphsForText('Queue', 120, 38),
      ...glyphsForText('21.8%', 340, 38)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const blocks = index.pageMap.get(1).blocks.filter((block) => block.type === 'table');
  assert.equal(blocks.length, 1);
  assert.equal(blocks[0].units.length, 2);
});

test('justified paragraph gaps do not make a table block', () => {
  const page = {
    pageNumber: 1,
    width: 520,
    height: 180,
    glyphs: [
      ...glyphsForWords(['Network', 'simulation', 'evaluates', 'protocols'], [20, 135, 260, 390], 20),
      ...glyphsForWords(['Packet', 'level', 'simulators', 'remain'], [20, 122, 230, 380], 36),
      ...glyphsForWords(['Framework', 'machinery', 'adds', 'overhead'], [20, 150, 285, 410], 52)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const blocks = index.pageMap.get(1).blocks;
  assert.equal(blocks.filter((block) => block.type === 'table').length, 0);
  assert.equal(blocks.filter((block) => block.type === 'paragraph').length, 1);
});

test('indented paragraph starts create separate blocks', () => {
  const page = {
    pageNumber: 1,
    width: 520,
    height: 180,
    glyphs: [
      ...glyphsForText('Network simulation is indispensable for evaluating protocols', 20, 20),
      ...glyphsForText('and architectures, yet researchers face a dilemma.', 20, 36),
      ...glyphsForText('We present SimFactory, a system that generates simulators', 44, 54),
      ...glyphsForText('from structured specifications and validates outputs.', 20, 70)
    ]
  };
  const index = PdfSelectionIndex.fromPages([page]);
  const blocks = index.pageMap.get(1).blocks.filter((block) => block.type === 'paragraph');
  assert.equal(blocks.length, 2);
  assert.equal(blocks[0].units.length, 2);
  assert.equal(blocks[1].units.length, 2);
});

function glyphsForText(text, x, y, widths = []) {
  const glyphs = [];
  let left = x;
  for (const [index, char] of Array.from(text).entries()) {
    const width = widths[index] || 8;
    glyphs.push({
      char,
      rect: { left, top: y, width, height: 10 },
      tightRect: { left: left + 0.5, top: y + 1, width: Math.max(1, width - 1), height: 8 }
    });
    left += width;
  }
  return glyphs;
}

function glyphsForWords(words, starts, y) {
  return words.flatMap((word, index) => glyphsForText(word, starts[index], y));
}

function glyphsForTextWithRects(text, x, looseY, looseHeight, tightY, tightHeight) {
  const glyphs = [];
  let left = x;
  for (const char of Array.from(text)) {
    glyphs.push(glyphWithRects(char, left, looseY, looseHeight, tightY, tightHeight));
    left += 8;
  }
  return glyphs;
}

function glyphWithRects(char, left, looseY, looseHeight, tightY, tightHeight) {
  return {
    char,
    rect: { left, top: looseY, width: 8, height: looseHeight },
    tightRect: { left: left + 0.5, top: tightY, width: 7, height: tightHeight }
  };
}

function glyphGeometryForText(text, startX = 10, y = 10) {
  const glyphs = [];
  let x = startX;
  for (const char of Array.from(text)) {
    if (char === ' ') {
      x += 10;
      glyphs.push({
        x,
        y,
        width: 0,
        height: 10,
        tightX: x,
        tightY: y + 1,
        tightWidth: 0,
        tightHeight: 8,
        flags: 1
      });
      continue;
    }
    glyphs.push({
      x,
      y,
      width: 6,
      height: 10,
      tightX: x + 0.5,
      tightY: y + 1,
      tightWidth: 5,
      tightHeight: 8,
      flags: 0
    });
    x += 6;
  }
  return glyphs;
}
