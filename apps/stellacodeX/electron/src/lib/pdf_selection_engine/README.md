# PDF Selection Engine Prototype

This is an isolated prototype for replacing run-level PDF text selection with a glyph-level selection core.

## Why this exists

The current Electron PDF preview renders pages through PDFium, but selection is derived from text-run bounding boxes and a linear `clientX / runWidth` character estimate. That is fast to write, but it is inaccurate for proportional fonts, ligatures, formulas, CJK, rotated text, and multi-column papers.

This prototype treats PDF text selection as a geometry problem:

- consume glyph-level boxes, preferring tight boxes for hit-testing;
- build page-local spatial bins so pointer moves do not scan the whole document;
- infer line segments and column lanes once during indexing;
- select by glyph order boundaries instead of estimating offsets inside a run;
- emit selected text plus merged highlight rectangles.

## Intended Electron integration

Use EmbedPDF/PDFium direct engine as the extractor:

```js
import { PdfSelectionIndex, glyphPageFromEmbedPdf } from './src/index.js';

const geometry = await taskToPromise(engine.getPageGeometry(doc, page));
const textRuns = await taskToPromise(engine.getPageTextRuns(doc, page));
const selectionPage = glyphPageFromEmbedPdf({
  page,
  geometry,
  textRuns,
  scale: scaleFactor
});

const index = PdfSelectionIndex.fromPages([selectionPage]);
const anchor = index.hitTest({ pageNumber: 1, x, y });
const focus = index.hitTest({ pageNumber: 1, x: endX, y: endY });
const selection = index.selectBetween(anchor, focus);
```

`selection` contains:

- `selectedText`
- `rects`
- `pages`
- `start` / `end` glyph-order boundaries

## Local checks

```bash
npm test
node scripts/bench-main-pdf.mjs ~/Downloads/main.pdf
```

The benchmark uses Poppler TSV as a stand-in extractor when running outside Electron. It expands word boxes into pseudo-glyphs, so it only validates indexing and selection cost. In Electron, wire it to `getPageGeometry()` for real glyph boxes.
