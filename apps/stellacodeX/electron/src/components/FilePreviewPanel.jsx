import { Component, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import rehypeRaw from 'rehype-raw';
import remarkGfm from 'remark-gfm';
import hljs from 'highlight.js';
import mammoth from 'mammoth/mammoth.browser';
import { renderAsync as renderDocxAsync } from 'docx-preview';
import embedPdfiumWasmUrl from '@embedpdf/pdfium/pdfium.wasm?url';
import { Bug, Code2, Download, Eye, FileText, Info, Printer, RefreshCw, X } from 'lucide-react';
import stellacodeMark from '../assets/stellacode-mark.svg';
import { handleExternalLinkClick, isExternalUrl } from '../lib/externalLinks';
import { fileExtension, fileNameFromPath, imageMimeType, isHtmlFile, isImageFile, isMarkdownFile, isPdfFile, isPresentationFile, isWordFile } from '../lib/fileUtils';
import { PDF_SELECTION_ENGINE_VERSION, PdfSelectionIndex, glyphPageFromEmbedPdf } from '../lib/pdf_selection_engine/src/index.js';

const PDF_ENGINE_KIND = 'stella-pdfium-glyph';
const PDF_WORKER_ENGINE_LABEL = 'Stella PDFium Worker';
const PDF_DIRECT_ENGINE_LABEL = 'Stella PDFium Glyph';
const PDF_RENDER_WORKER_COUNT = 2;
const PDF_RENDER_CONCURRENCY = 2;

function createEmptyPdfPerfStats() {
  return {
    pages: {},
    renderedPages: 0,
    paintedPages: 0,
    renderMsTotal: 0,
    geometryMsTotal: 0,
    paintMsTotal: 0,
    totalMsTotal: 0,
    maxTotalMs: null,
    lastPage: null,
    lastPixels: ''
  };
}

function updatePdfPerfStats(previous, sample) {
  const pages = {
    ...previous.pages,
    [sample.page]: {
      ...(previous.pages[sample.page] || {}),
      ...sample
    }
  };
  const rendered = Object.values(pages).filter((page) => Number.isFinite(page.totalMs));
  const painted = Object.values(pages).filter((page) => Number.isFinite(page.paintMs));
  const sum = (key) => rendered.reduce((total, page) => total + (Number.isFinite(page[key]) ? page[key] : 0), 0);
  return {
    pages,
    renderedPages: rendered.length,
    paintedPages: painted.length,
    renderMsTotal: sum('renderMs'),
    geometryMsTotal: sum('geometryMs'),
    paintMsTotal: sum('paintMs'),
    totalMsTotal: sum('totalMs'),
    maxTotalMs: rendered.length > 0 ? rendered.reduce((max, page) => Math.max(max, Number.isFinite(page.totalMs) ? page.totalMs : 0), 0) : null,
    lastPage: sample.page,
    lastPixels: sample.pixels || previous.lastPixels || ''
  };
}

function averagePdfMs(total, count) {
  return count > 0 ? `${Math.round(total / count)} ms` : '-';
}

function formatPdfMs(value) {
  return Number.isFinite(value) ? `${Math.round(value)} ms` : '-';
}

function pdfPageMetaFromPage(page, availableWidth, zoomPercent, existing = {}) {
  const basePadding = Math.min(10, Math.max(4, availableWidth * 0.01));
  const fitScale = Math.max(0.2, Math.min(3, (availableWidth - basePadding * 2) / Math.max(1, page.size.width)));
  const scaleFactor = fitScale * (zoomPercent / 100);
  return {
    ...existing,
    pageNumber: page.index + 1,
    width: Math.max(1, Math.round(page.size.width * scaleFactor)),
    height: Math.max(1, Math.round(page.size.height * scaleFactor)),
    scaleFactor,
    url: existing.url || '',
    imageKey: existing.imageKey || '',
    textRuns: existing.textRuns || [],
    rendered: Boolean(existing.rendered),
    rendering: Boolean(existing.rendering)
  };
}

function updatePdfSelectionPage(runtime, page, meta, textGeometry) {
  if (!runtime || !page || !meta || !textGeometry?.geometry || !textGeometry?.pageText) return false;
  runtime.selectionPages.set(meta.pageNumber, glyphPageFromEmbedPdf({
    page,
    geometry: textGeometry.geometry,
    textRuns: textGeometry.pageText,
    pageIndex: page.index,
    scale: meta.scaleFactor
  }));
  return true;
}

function buildPdfSelectionIndexFromRuntime(runtime) {
  const selectionPages = Array.from(runtime?.selectionPages?.values?.() || [])
    .sort((a, b) => a.pageNumber - b.pageNumber);
  return selectionPages.length > 0 ? PdfSelectionIndex.fromPages(selectionPages) : null;
}

export function FilePreviewPanel({ open, openFiles, activeFilePath, onSelectFile, onCloseFile, onDownloadFile, onRefreshPdfPreview, onResolveMarkdownAsset, onCreateSelectionReference }) {
  const activeFile = openFiles.find((file) => file.path === activeFilePath) || null;
  const [selectionMenu, setSelectionMenu] = useState(null);

  useEffect(() => {
    if (!selectionMenu) return undefined;
    const close = () => setSelectionMenu(null);
    const closeOnEscape = (event) => {
      if (event.key === 'Escape') close();
    };
    window.addEventListener('pointerdown', close);
    window.addEventListener('keydown', closeOnEscape);
    return () => {
      window.removeEventListener('pointerdown', close);
      window.removeEventListener('keydown', closeOnEscape);
    };
  }, [selectionMenu]);

  const openSelectionMenu = (event, reference) => {
    if (!reference?.selected_text) return;
    event.preventDefault();
    event.stopPropagation();
    setSelectionMenu({
      x: event.clientX,
      y: event.clientY,
      reference
    });
  };

  const quoteSelection = () => {
    if (selectionMenu?.reference) {
      onCreateSelectionReference?.(selectionMenu.reference);
    }
    setSelectionMenu(null);
  };

  const copySelection = () => {
    const text = selectionMenu?.reference?.selected_text || '';
    if (text) navigator.clipboard?.writeText(text).catch(() => {});
    setSelectionMenu(null);
  };

  return (
    <aside className={`right-panel preview-panel${open ? ' open' : ''}`} aria-hidden={!open}>
      <section className="file-preview detached">
        <div className="editor-tabs">
          {openFiles.map((file) => (
            <div
              key={file.path}
              className={`editor-tab${activeFile?.path === file.path ? ' active' : ''}`}
              role="button"
              tabIndex={0}
              onClick={() => onSelectFile(file.path)}
              onKeyDown={(event) => {
                if (event.key === 'Enter' || event.key === ' ') {
                  event.preventDefault();
                  onSelectFile(file.path);
                }
              }}
            >
              <span>{file.name}</span>
              <button
                className="editor-tab-close"
                type="button"
                aria-label={`关闭 ${file.name}`}
                onClick={(event) => {
                  event.stopPropagation();
                  onCloseFile(file.path);
                }}
              >
                <X size={12} />
              </button>
            </div>
          ))}
        </div>
        <div
          className="preview-surface"
          onPointerDownCapture={preserveSelectionOnRightPointerDown}
          onContextMenuCapture={(event) => handlePreviewSurfaceContextMenu(event, activeFile, openSelectionMenu)}
        >
          {activeFile?.loading ? (
            <div className="panel-placeholder">正在读取文件...</div>
          ) : activeFile?.error ? (
            <div className="panel-placeholder">{activeFile.error}</div>
          ) : activeFile ? (
            <PreviewErrorBoundary resetKey={activeFile.path}>
              <FilePreview file={activeFile} onDownloadFile={onDownloadFile} onRefreshPdfPreview={onRefreshPdfPreview} onResolveMarkdownAsset={onResolveMarkdownAsset} onSelectionContextMenu={openSelectionMenu} />
            </PreviewErrorBoundary>
          ) : (
            <div className="panel-placeholder">打开一个文件查看预览</div>
          )}
        </div>
        {selectionMenu && createPortal(
          <div
            className="selection-context-menu"
            style={selectionMenuStyle(selectionMenu)}
            onPointerDown={(event) => event.stopPropagation()}
            onContextMenu={(event) => event.preventDefault()}
          >
            <button type="button" onClick={quoteSelection}>
              <FileText size={13} />
              引用到对话
            </button>
            <button type="button" onClick={copySelection}>
              复制选中内容
            </button>
          </div>,
          document.body
        )}
      </section>
    </aside>
  );
}

class PreviewErrorBoundary extends Component {
  constructor(props) {
    super(props);
    this.state = { error: null, resetKey: props.resetKey };
  }

  static getDerivedStateFromError(error) {
    return { error };
  }

  static getDerivedStateFromProps(props, state) {
    if (props.resetKey !== state.resetKey) {
      return { error: null, resetKey: props.resetKey };
    }
    return null;
  }

  render() {
    if (this.state.error) {
      return <div className="panel-placeholder">预览失败：{this.state.error?.message || '文件渲染异常'}</div>;
    }
    return this.props.children;
  }
}

function FilePreview({ file, onDownloadFile, onRefreshPdfPreview, onResolveMarkdownAsset, onSelectionContextMenu }) {
  const name = file.name || fileNameFromPath(file.path);
  const ext = fileExtension(name);
  const source = file.content || file.data || '';
  if (file.kind === 'pdf' || isPdfFile(name)) {
    if (file.pdf_url) {
      return <PdfPreview file={file} name={name} onDownloadFile={onDownloadFile} onRefreshPdfPreview={onRefreshPdfPreview} onSelectionContextMenu={onSelectionContextMenu} />;
    }
    return (
      <div className="file-binary-preview">
        <FileText size={34} />
        <strong>{name}</strong>
        <span>无法在面板内加载这个 PDF，可以下载后查看。</span>
        {onRefreshPdfPreview ? (
          <button
            className="secondary-button"
            type="button"
            onClick={() => onRefreshPdfPreview?.(file)}
          >
            <RefreshCw size={14} />
            重新加载 PDF
          </button>
        ) : null}
        <button
          className="secondary-button"
          type="button"
          onClick={() => onDownloadFile?.(file)}
        >
          <Download size={14} />
          下载 PDF
        </button>
      </div>
    );
  }
  const rawImageSource = file.url || file.data_url || file.uri || file.file_uri || (file.encoding === 'base64' ? `data:${imageMimeType(name)};base64,${file.data || file.content || ''}` : '');
  const imageSource = /^(https?:|data:|blob:)/i.test(rawImageSource) ? rawImageSource : '';
  const canRenderImage = (file.kind === 'image' || isImageFile(name)) && ext !== 'svg';
  const canRenderSvgImage = ext === 'svg' && imageSource;
  if (canRenderImage || canRenderSvgImage) {
    return (
      <div className="file-image-preview">
        <img src={imageSource || stellacodeMark} alt={name} loading="lazy" />
        <span>{name}</span>
      </div>
    );
  }
  if (file.kind === 'markdown' || isMarkdownFile(name)) {
    return (
      <article
        className="markdown-preview"
        data-selection-kind="markdown"
        onContextMenu={(event) => handleDomSelectionContextMenu(event, event.currentTarget, file, 'markdown', source, onSelectionContextMenu)}
      >
        <MarkdownBlock text={source} path={file.path} onResolveAsset={onResolveMarkdownAsset} />
      </article>
    );
  }
  if (file.kind === 'html' || isHtmlFile(name)) {
    return <HtmlPreview file={file} name={name} source={source} language={file.language || ext} onSelectionContextMenu={onSelectionContextMenu} />;
  }
  if (file.kind === 'word' || isWordFile(name)) {
    return (
      <WordPreview
        file={file}
        name={name}
        source={source}
        onSelectionContextMenu={onSelectionContextMenu}
      />
    );
  }
  if (file.kind === 'presentation' || isPresentationFile(name)) {
    return (
      <BinaryPreview
        file={file}
        icon={<FileText size={34} />}
        title={name}
        message="PPT/PPTX 暂不支持内嵌预览，可以下载后用系统演示文稿应用打开。"
        actionLabel="下载演示文稿"
        onDownloadFile={onDownloadFile}
      />
    );
  }
  if (file.encoding && file.encoding !== 'utf8') {
    return (
      <BinaryPreview
        file={file}
        icon={<FileText size={34} />}
        title={name}
        message="这个文件不是文本格式，已跳过源码渲染以避免界面卡顿。"
        actionLabel="下载文件"
        onDownloadFile={onDownloadFile}
      />
    );
  }
  return (
    <CodePreview file={file} code={source} language={file.language || ext} onSelectionContextMenu={onSelectionContextMenu} />
  );
}

function BinaryPreview({ file, icon, title, message, actionLabel, onDownloadFile }) {
  return (
    <div className="file-binary-preview">
      {icon}
      <strong>{title}</strong>
      <span>{message}</span>
      {onDownloadFile ? (
        <button
          className="secondary-button"
          type="button"
          onClick={() => onDownloadFile?.(file)}
        >
          <Download size={14} />
          {actionLabel}
        </button>
      ) : null}
    </div>
  );
}

function HtmlPreview({ file, name, source, language, onSelectionContextMenu }) {
  const [mode, setMode] = useState('render');

  useEffect(() => {
    setMode('render');
  }, [name, source]);

  return (
    <div className="html-preview">
      <div className="html-preview-toolbar">
        <strong>{name}</strong>
        <div className="preview-mode-toggle" role="tablist" aria-label="HTML preview mode">
          <button
            className={mode === 'render' ? 'active' : ''}
            type="button"
            role="tab"
            aria-selected={mode === 'render'}
            onClick={() => setMode('render')}
          >
            <Eye size={13} />
            预览
          </button>
          <button
            className={mode === 'source' ? 'active' : ''}
            type="button"
            role="tab"
            aria-selected={mode === 'source'}
            onClick={() => setMode('source')}
          >
            <Code2 size={13} />
            源码
          </button>
        </div>
      </div>
      <div className="html-preview-body">
        {mode === 'render' ? (
          <iframe
            className="html-render-frame"
            title={name}
            sandbox=""
            referrerPolicy="no-referrer"
            srcDoc={source}
          />
        ) : (
          <CodePreview file={file} code={source} language={language || 'html'} sourceKind="html" onSelectionContextMenu={onSelectionContextMenu} />
        )}
      </div>
    </div>
  );
}

function PdfPreview({ file, name, onDownloadFile, onRefreshPdfPreview, onSelectionContextMenu }) {
  const shellRef = useRef(null);
  const renderUrlsRef = useRef([]);
  const scrollHintAppliedRef = useRef('');
  const pdfDragRef = useRef(null);
  const pdfSelectionRef = useRef(null);
  const pdfSelectionIndexRef = useRef(null);
  const pdfRuntimeRef = useRef(null);
  const pdfSelectionFrameRef = useRef(0);
  const pdfSelectionFinalizeTimerRef = useRef(0);
  const pdfRenderScheduleRef = useRef(0);
  const pdfRenderPumpTimerRef = useRef(0);
  const pdfPerfFrameRef = useRef(0);
  const pdfPerfPendingRef = useRef([]);
  const paintedImageKeysRef = useRef(new Set());
  const [wasmUrl, setWasmUrl] = useState('');
  const [wasmError, setWasmError] = useState('');
  const [debugEntries, setDebugEntries] = useState([]);
  const [shellWidth, setShellWidth] = useState(0);
  const zoomPercent = 100;
  const [showInfoPanel, setShowInfoPanel] = useState(false);
  const [showPdfSelectionDebug, setShowPdfSelectionDebug] = useState(false);
  const [activePdfEngineLabel, setActivePdfEngineLabel] = useState(PDF_WORKER_ENGINE_LABEL);
  const [pdfPerfStats, setPdfPerfStats] = useState(() => createEmptyPdfPerfStats());
  const [pdfSelectionRects, setPdfSelectionRects] = useState([]);
  const [pdfSelectionDebugSnapshot, setPdfSelectionDebugSnapshot] = useState(null);
  const [pdfSelectionDebugDetail, setPdfSelectionDebugDetail] = useState(null);
  const [renderState, setRenderState] = useState({
    loading: true,
    pages: [],
    pageCount: 0,
    error: ''
  });
  const documentId = useMemo(() => `preview-${stableHash(file.path || name)}`, [file.path, name]);
  const pdfBuffer = useMemo(() => arrayBufferFromPdfFile(file), [file]);
  const pdfSelectionRectsByPage = useMemo(() => {
    const byPage = new Map();
    for (const rect of pdfSelectionRects) {
      const pageRects = byPage.get(rect.pageNumber);
      if (pageRects) {
        pageRects.push(rect);
      } else {
        byPage.set(rect.pageNumber, [rect]);
      }
    }
    return byPage;
  }, [pdfSelectionRects]);
  const pdfSelectionDebugByPage = useMemo(() => {
    const byPage = new Map();
    for (const page of pdfSelectionDebugSnapshot?.pages || []) {
      byPage.set(page.pageNumber, page);
    }
    return byPage;
  }, [pdfSelectionDebugSnapshot]);
  const addDebugEntry = (level, message, detail) => {
    const entry = {
      at: new Date().toLocaleTimeString(),
      level,
      message,
      detail: detail ? stringifyDebugDetail(detail) : ''
    };
    setDebugEntries((entries) => [...entries.slice(-29), entry]);
    if (level === 'error') {
      console.error('[StellaCodeX PDF]', message, detail || '');
    } else if (level === 'warn') {
      console.warn('[StellaCodeX PDF]', message, detail || '');
    }
  };
  const recordPdfPerfSample = useCallback((sample) => {
    pdfPerfPendingRef.current.push(sample);
    if (pdfPerfFrameRef.current) return;
    pdfPerfFrameRef.current = requestAnimationFrame(() => {
      pdfPerfFrameRef.current = 0;
      const samples = pdfPerfPendingRef.current;
      pdfPerfPendingRef.current = [];
      if (samples.length === 0) return;
      setPdfPerfStats((current) => samples.reduce(updatePdfPerfStats, current));
    });
  }, []);
  const handlePdfCanvasPaint = useCallback((pageNumber, imageKey, paintMs) => {
    if (!imageKey || paintedImageKeysRef.current.has(imageKey)) return;
    paintedImageKeysRef.current.add(imageKey);
    recordPdfPerfSample({ page: pageNumber, paintMs });
  }, [recordPdfPerfSample]);

  useEffect(() => {
    let disposed = false;
    setWasmUrl('');
    setWasmError('');
    setDebugEntries([]);
    setActivePdfEngineLabel(PDF_WORKER_ENGINE_LABEL);
    setPdfPerfStats(createEmptyPdfPerfStats());
    pdfPerfPendingRef.current = [];
    if (pdfPerfFrameRef.current) {
      cancelAnimationFrame(pdfPerfFrameRef.current);
      pdfPerfFrameRef.current = 0;
    }
    paintedImageKeysRef.current.clear();
    pdfDragRef.current = null;
    pdfSelectionRef.current = null;
    pdfSelectionIndexRef.current = null;
    setPdfSelectionDebugSnapshot(null);
    setPdfSelectionDebugDetail(null);
    setPdfSelectionRects([]);
    addDebugEntry('info', 'Using Vite PDFium WASM asset URL', { url: embedPdfiumWasmUrl });
    if (!disposed) setWasmUrl(embedPdfiumWasmUrl);
    return () => {
      disposed = true;
    };
  }, [file.pdf_url]);

  useEffect(() => {
    const node = shellRef.current;
    if (!node) return undefined;
    let frameId = 0;
    let lastWidth = 0;
    const applyWidth = (width) => {
      if (!width || Math.abs(width - lastWidth) < 4) return;
      lastWidth = width;
      setShellWidth(width);
    };
    const update = () => {
      if (frameId) return;
      frameId = requestAnimationFrame(() => {
        frameId = 0;
        const width = Math.max(0, Math.floor(node.clientWidth || 0));
        applyWidth(width);
      });
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => {
      if (frameId) cancelAnimationFrame(frameId);
      observer.disconnect();
    };
  }, []);

  useEffect(() => {
    const handleCopy = (event) => {
      const text = pdfSelectionRef.current?.selectedText;
      if (!text) return;
      event.clipboardData?.setData('text/plain', text);
      event.preventDefault();
    };
    document.addEventListener('copy', handleCopy);
    return () => document.removeEventListener('copy', handleCopy);
  }, []);

  useEffect(() => {
    const onError = (event) => {
      addDebugEntry('error', 'Window error during PDF preview', {
        message: event.message,
        filename: event.filename,
        lineno: event.lineno,
        colno: event.colno,
        error: event.error
      });
    };
    const onUnhandledRejection = (event) => {
      addDebugEntry('error', 'Unhandled promise rejection during PDF preview', event.reason);
    };
    window.addEventListener('error', onError);
    window.addEventListener('unhandledrejection', onUnhandledRejection);
    return () => {
      window.removeEventListener('error', onError);
      window.removeEventListener('unhandledrejection', onUnhandledRejection);
    };
  }, []);

  useEffect(() => {
    if (!wasmUrl || !pdfBuffer) return undefined;
    let disposed = false;
    let engine = null;
    const revision = (pdfRuntimeRef.current?.revision || 0) + 1;
    const availableWidth = shellRef.current?.clientWidth || shellWidth || 820;
    const load = async () => {
      const previousUrls = renderUrlsRef.current;
      setRenderState((current) => ({ ...current, loading: true, error: '' }));
      setPdfPerfStats(createEmptyPdfPerfStats());
      paintedImageKeysRef.current.clear();
      closePdfRuntime(pdfRuntimeRef.current);
      pdfRuntimeRef.current = null;
      pdfSelectionIndexRef.current = null;
      try {
        const openWithEngine = async (mode) => {
          const label = mode === 'worker' ? PDF_WORKER_ENGINE_LABEL : PDF_DIRECT_ENGINE_LABEL;
          const engineWasmUrl = absoluteUrlForWorker(wasmUrl);
          addDebugEntry('info', `Creating ${label}`, {
            wasmUrl: engineWasmUrl,
            pdfBytes: pdfBuffer.byteLength,
            shellWidth: availableWidth
          });
          const { createPdfiumEngine } = mode === 'worker'
            ? await import('@embedpdf/engines/pdfium-worker-engine')
            : await import('@embedpdf/engines/pdfium-direct-engine');
          const nextEngine = await withTimeout(
            Promise.resolve(createPdfiumEngine(engineWasmUrl, mode === 'worker' ? { encoderPoolSize: 0, fontFallback: null } : undefined)),
            20_000,
            `${label} initialization timed out`
          );
          if (disposed) return null;
          engine = nextEngine;
          addDebugEntry('info', `${label} created`);
          addDebugEntry('info', 'Opening PDF document buffer', { documentId, pdfBytes: pdfBuffer.byteLength, engine: label });
          const nextDoc = await taskToPromise(
            nextEngine.openDocumentBuffer({
              id: documentId,
              content: pdfBuffer.slice(0)
            }, { normalizeRotation: true }),
            `openDocumentBuffer:${mode}`,
            mode === 'worker' ? 12_000 : 20_000
          );
          return { engine: nextEngine, doc: nextDoc, label, mode };
        };

        let opened = null;
        try {
          opened = await openWithEngine('worker');
        } catch (workerError) {
          addDebugEntry('warn', 'PDFium worker engine failed; falling back to direct engine', workerError);
          closePdfEngine(engine);
          engine = null;
        }
        if (!opened && !disposed) {
          opened = await openWithEngine('direct');
        }
        if (!opened) return;
        engine = opened.engine;
        const doc = opened.doc;
        setActivePdfEngineLabel(opened.label);
        if (disposed) return;
        addDebugEntry('info', 'PDF document opened', { pageCount: doc.pageCount, engine: opened.label });
        const nextPages = doc.pages.map((page) => pdfPageMetaFromPage(page, availableWidth, zoomPercent));
        pdfRuntimeRef.current = {
          revision,
          engine,
          doc,
          disposed: false,
          pageByNumber: new Map(doc.pages.map((page) => [page.index + 1, page])),
          pageMetaByNumber: new Map(nextPages.map((page) => [page.pageNumber, page])),
          renderedUrls: [],
          pageImageByNumber: new Map(),
          renderWorkers: [createPdfRenderWorker(engine, doc, true)],
          imageSeq: 0,
          rendering: new Set(),
          selectionPages: new Map(),
          textGeometryByPage: new Map(),
          textGeometryQueued: new Set(),
          textGeometryTimers: new Map(),
          renderQueue: [],
          renderQueued: new Set(),
          renderPumpTimer: 0,
          pendingPageMeta: new Map(),
          pageMetaCommitFrame: 0,
          selectionIndexTimer: 0,
          layoutSeq: 0,
          deferRenderUntil: 0
        };
        renderUrlsRef.current = pdfRuntimeRef.current.renderedUrls;
        addDebugEntry('info', 'PDF document ready; pages will render lazily', {
          pageCount: doc.pageCount,
          engine: PDF_ENGINE_KIND,
          pagePlaceholders: nextPages.length
        });
        setRenderState({
          loading: false,
          pages: nextPages,
          pageCount: doc.pageCount,
          error: ''
        });
        revokeObjectUrls(previousUrls);
        if (opened.mode === 'worker') {
          warmPdfRenderWorkers(pdfRuntimeRef.current, {
            wasmUrl: absoluteUrlForWorker(wasmUrl),
            pdfBuffer,
            documentId,
            targetCount: PDF_RENDER_WORKER_COUNT,
            addDebugEntry,
            scheduleRender: () => schedulePdfRenderPump(pdfRuntimeRef.current)
          });
        }
        requestAnimationFrame(() => scheduleVisiblePdfRender());
      } catch (error) {
        if (!disposed) {
          addDebugEntry('error', 'PDFium render failed', error);
          setRenderState((current) => ({
            ...current,
            loading: false,
            error: error?.message || 'PDFium 渲染失败'
          }));
        }
      }
    };
    load();
    return () => {
      disposed = true;
      if (pdfRuntimeRef.current?.revision === revision) {
        closePdfRuntime(pdfRuntimeRef.current);
        pdfRuntimeRef.current = null;
      } else {
        closePdfEngine(engine);
      }
    };
  }, [documentId, pdfBuffer, wasmUrl, zoomPercent, recordPdfPerfSample]);

  useEffect(() => () => {
    if (pdfSelectionFrameRef.current) {
      cancelAnimationFrame(pdfSelectionFrameRef.current);
      pdfSelectionFrameRef.current = 0;
    }
    if (pdfSelectionFinalizeTimerRef.current) {
      cancelIdleCallbackSafe(pdfSelectionFinalizeTimerRef.current);
      pdfSelectionFinalizeTimerRef.current = 0;
    }
    if (pdfRenderScheduleRef.current) {
      cancelAnimationFrame(pdfRenderScheduleRef.current);
      pdfRenderScheduleRef.current = 0;
    }
    if (pdfRenderPumpTimerRef.current) {
      window.clearTimeout(pdfRenderPumpTimerRef.current);
      pdfRenderPumpTimerRef.current = 0;
    }
    if (pdfPerfFrameRef.current) {
      cancelAnimationFrame(pdfPerfFrameRef.current);
      pdfPerfFrameRef.current = 0;
    }
    pdfPerfPendingRef.current = [];
    closePdfRuntime(pdfRuntimeRef.current);
    pdfRuntimeRef.current = null;
    revokeObjectUrls(renderUrlsRef.current);
    renderUrlsRef.current = [];
  }, []);

  useEffect(() => {
    const runtime = pdfRuntimeRef.current;
    if (!runtime || runtime.disposed || !runtime.doc || !shellWidth || renderState.loading) return;
    let changed = false;
    const nextPages = runtime.doc.pages.map((page) => {
      const pageNumber = page.index + 1;
      const previous = runtime.pageMetaByNumber.get(pageNumber) || {};
      const next = pdfPageMetaFromPage(page, shellWidth, zoomPercent, previous);
      const scaleChanged = Math.abs((previous.scaleFactor || 0) - next.scaleFactor) > 0.01;
      if (scaleChanged) {
        changed = true;
        next.needsRerender = Boolean(previous.imageKey);
        next.rendered = Boolean(previous.imageKey);
        next.rendering = false;
        if (!updatePdfSelectionPage(runtime, page, next, runtime.textGeometryByPage?.get(pageNumber))) {
          runtime.selectionPages.delete(pageNumber);
        }
      }
      runtime.pageMetaByNumber.set(pageNumber, next);
      return next;
    });
    if (!changed) return;
    runtime.layoutSeq = (runtime.layoutSeq || 0) + 1;
    runtime.deferRenderUntil = performance.now() + 900;
    runtime.renderQueue = [];
    runtime.renderQueued?.clear?.();
    schedulePdfSelectionIndexRebuild(runtime);
    if (!showPdfSelectionDebug) {
      pdfSelectionRef.current = null;
    }
    setPdfSelectionDebugSnapshot(null);
    setPdfSelectionDebugDetail(null);
    setPdfSelectionRects([]);
    setRenderState((current) => ({
      ...current,
      pages: nextPages
    }));
    window.setTimeout(() => scheduleVisiblePdfRender(), 920);
  }, [shellWidth, zoomPercent, renderState.loading]);

  useEffect(() => {
    const node = shellRef.current;
    if (!node) return undefined;
    const schedule = () => scheduleVisiblePdfRender();
    node.addEventListener('scroll', schedule, { passive: true });
    schedule();
    return () => {
      node.removeEventListener('scroll', schedule);
    };
  }, [renderState.pages.length]);

  useLayoutEffect(() => {
    const node = shellRef.current;
    const hint = file.scroll_hint;
    if (!node || !hint || renderState.loading || renderState.pages.length === 0) return;
    const hintKey = `${file.pdf_url || ''}:${hint.updated_at || ''}:${renderState.pages.length}`;
    if (scrollHintAppliedRef.current === hintKey) return;
    scrollHintAppliedRef.current = hintKey;
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        const maxScrollTop = Math.max(0, node.scrollHeight - node.clientHeight);
        const ratioTop = Number.isFinite(hint.ratio_top) ? hint.ratio_top * maxScrollTop : 0;
        const absoluteTop = Number.isFinite(hint.scroll_top) ? hint.scroll_top : ratioTop;
        node.scrollTop = Math.max(0, Math.min(maxScrollTop, maxScrollTop > 0 ? ratioTop : absoluteTop));
        if (Number.isFinite(hint.scroll_left)) {
          node.scrollLeft = Math.max(0, hint.scroll_left);
        }
      });
    });
  }, [file.pdf_url, file.scroll_hint, renderState.loading, renderState.pages.length]);

  const printPdf = () => {
    if (!file.pdf_url) return;
    const frame = document.createElement('iframe');
    frame.style.position = 'fixed';
    frame.style.right = '0';
    frame.style.bottom = '0';
    frame.style.width = '1px';
    frame.style.height = '1px';
    frame.style.border = '0';
    frame.style.opacity = '0';
    frame.src = pdfEmbedUrl(file.pdf_url);
    const cleanup = () => {
      window.setTimeout(() => frame.remove(), 1000);
    };
    frame.onload = () => {
      const frameWindow = frame.contentWindow;
      if (!frameWindow) {
        cleanup();
        return;
      }
      frameWindow.focus();
      frameWindow.print();
      cleanup();
    };
    document.body.appendChild(frame);
  };

  const handleContextMenu = (event) => {
    const reference = buildPdfSelectionReference(shellRef.current, file, pdfSelectionRef.current);
    if (reference) {
      onSelectionContextMenu?.(event, reference);
      return;
    }
    event.preventDefault();
    event.stopPropagation();
  };

  const handlePdfPointerDown = (event) => {
    if (event.button !== 0) return;
    const point = pdfPagePointFromEvent(event, event.currentTarget);
    if (!point) return;
    event.preventDefault();
    window.getSelection?.()?.removeAllRanges();
    pdfDragRef.current = {
      pointerId: event.pointerId,
      start: point,
      current: point,
      moved: false,
      engine: PDF_ENGINE_KIND,
      runs: null
    };
    if (!showPdfSelectionDebug) {
      pdfSelectionRef.current = null;
    }
    if (pdfSelectionFinalizeTimerRef.current) {
      cancelIdleCallbackSafe(pdfSelectionFinalizeTimerRef.current);
      pdfSelectionFinalizeTimerRef.current = 0;
    }
    event.currentTarget.setPointerCapture?.(event.pointerId);
  };

  const applyPdfDragSelection = (drag, current) => {
    const shell = shellRef.current;
    if (!drag || !shell) return;
    const index = currentPdfSelectionIndex();
    const selection = buildPdfGlyphDragSelection(index, drag.start, current, {
      includeText: false
    });
    pdfSelectionRef.current = selection;
    setPdfSelectionRects(selection?.rects || []);
    if (showPdfSelectionDebug) {
      setPdfSelectionDebugSnapshot(buildPdfSelectionDebugSnapshot(index, selection, {
        anchorPoint: drag.start,
        focusPoint: current,
        phase: 'drag'
      }));
      setPdfSelectionDebugDetail(null);
    }
  };

  const schedulePdfSelectionFinalize = (drag, current) => {
    if (!drag || !current) return;
    if (pdfSelectionFinalizeTimerRef.current) {
      cancelIdleCallbackSafe(pdfSelectionFinalizeTimerRef.current);
      pdfSelectionFinalizeTimerRef.current = 0;
    }
    pdfSelectionFinalizeTimerRef.current = requestIdleCallbackSafe(() => {
      pdfSelectionFinalizeTimerRef.current = 0;
      const index = currentPdfSelectionIndex();
      const selection = buildPdfGlyphDragSelection(index, drag.start, current, {
        includeText: true
      });
      if (!selection) return;
      pdfSelectionRef.current = selection;
      setPdfSelectionRects(selection.rects || []);
      if (showPdfSelectionDebug) {
        setPdfSelectionDebugSnapshot(buildPdfSelectionDebugSnapshot(index, selection, {
          anchorPoint: drag.start,
          focusPoint: current,
          phase: 'finalize'
        }));
        setPdfSelectionDebugDetail(null);
      }
    });
  };

  const schedulePdfSelectionIndexRebuild = (runtime) => {
    if (!runtime || runtime.disposed || runtime.selectionIndexTimer) return;
    runtime.selectionIndexTimer = requestIdleCallbackSafe(() => {
      runtime.selectionIndexTimer = 0;
      if (runtime.disposed) return;
      pdfSelectionIndexRef.current = buildPdfSelectionIndexFromRuntime(runtime);
    });
  };

  const currentPdfSelectionIndex = () => {
    if (pdfSelectionIndexRef.current?.engineVersion === PDF_SELECTION_ENGINE_VERSION) {
      return pdfSelectionIndexRef.current;
    }
    const runtime = pdfRuntimeRef.current;
    if (!runtime || runtime.disposed) return pdfSelectionIndexRef.current;
    pdfSelectionIndexRef.current = buildPdfSelectionIndexFromRuntime(runtime);
    return pdfSelectionIndexRef.current;
  };

  const togglePdfSelectionDebug = () => {
    setShowPdfSelectionDebug((enabled) => {
      const next = !enabled;
      if (next) {
        const index = currentPdfSelectionIndex();
        setPdfSelectionDebugSnapshot(buildPdfSelectionDebugSnapshot(index, pdfSelectionRef.current, {
          phase: 'manual'
        }));
      } else {
        setPdfSelectionDebugSnapshot(null);
        setPdfSelectionDebugDetail(null);
      }
      return next;
    });
  };

  const inspectPdfSelectionDebugAtPoint = (point) => {
    const index = currentPdfSelectionIndex();
    if (!index || !point) return;
    const snapshot = buildPdfSelectionDebugSnapshot(index, pdfSelectionRef.current, {
      focusPoint: point,
      phase: 'inspect'
    });
    setPdfSelectionDebugSnapshot(snapshot);
    setPdfSelectionDebugDetail(findPdfSelectionDebugItem(snapshot, point));
  };

  const schedulePdfTextGeometry = (runtime, pageNumber, page, meta, layoutSeq, renderMs) => {
    if (!runtime || runtime.disposed || runtime.textGeometryQueued?.has(pageNumber)) return;
    runtime.textGeometryQueued.add(pageNumber);
    const timerId = requestIdleCallbackSafe(() => {
      runtime.textGeometryTimers?.delete(pageNumber);
      processPdfTextGeometry(runtime, pageNumber, page, meta, layoutSeq, renderMs).finally(() => {
        runtime.textGeometryQueued?.delete(pageNumber);
      });
    });
    runtime.textGeometryTimers?.set(pageNumber, timerId);
  };

  const schedulePdfPageMetaCommit = (runtime, pageNumber, meta) => {
    if (!runtime || runtime.disposed) return;
    runtime.pageMetaByNumber.set(pageNumber, meta);
    runtime.pendingPageMeta?.set(pageNumber, meta);
    if (runtime.pageMetaCommitFrame) return;
    runtime.pageMetaCommitFrame = requestAnimationFrame(() => {
      runtime.pageMetaCommitFrame = 0;
      const pending = runtime.pendingPageMeta;
      runtime.pendingPageMeta = new Map();
      if (runtime.disposed || !pending || pending.size === 0) return;
      setRenderState((current) => ({
        ...current,
        pages: current.pages.map((item) => pending.get(item.pageNumber) || item)
      }));
    });
  };

  const processPdfTextGeometry = async (runtime, pageNumber, page, meta, layoutSeq, renderMs) => {
    if (!runtime || runtime.disposed || runtime.layoutSeq !== layoutSeq) return;
    let pageText = null;
    let geometryMode = 'none';
    let geometryMs = 0;
    const cachedGeometry = runtime.textGeometryByPage?.get(pageNumber);
    if (cachedGeometry && updatePdfSelectionPage(runtime, page, meta, cachedGeometry)) {
      geometryMode = `${cachedGeometry.mode || 'fast-glyph'}-cache`;
      pageText = cachedGeometry.pageText;
      schedulePdfSelectionIndexRebuild(runtime);
    } else {
      try {
        const geometryStartAt = performance.now();
        const fastGeometry = await taskToPromise(
          runtime.engine.getPageTextGeometry(runtime.doc, page),
          `getPageTextGeometry:${pageNumber}`,
          15_000
        );
        geometryMs = performance.now() - geometryStartAt;
        geometryMode = 'fast-glyph';
        pageText = fastGeometry.pageText;
        const textGeometry = {
          geometry: fastGeometry.geometry,
          pageText,
          mode: geometryMode
        };
        runtime.textGeometryByPage?.set(pageNumber, textGeometry);
        updatePdfSelectionPage(runtime, page, meta, textGeometry);
        schedulePdfSelectionIndexRebuild(runtime);
      } catch (fastGeometryError) {
        addDebugEntry('warn', 'Fast PDF glyph geometry failed; falling back', { page: pageNumber, error: fastGeometryError });
      }
    }
    if (!geometryMode || geometryMode === 'none') {
      try {
        const geometryStartAt = performance.now();
        pageText = await taskToPromise(
          runtime.engine.getPageTextRuns(runtime.doc, page),
          `getPageTextRuns:${pageNumber}`,
          15_000
        );
        const geometry = await taskToPromise(
          runtime.engine.getPageGeometry(runtime.doc, page),
          `getPageGeometry:${pageNumber}`,
          15_000
        );
        geometryMs = performance.now() - geometryStartAt;
        geometryMode = 'fallback-glyph';
        const textGeometry = {
          geometry,
          pageText,
          mode: geometryMode
        };
        runtime.textGeometryByPage?.set(pageNumber, textGeometry);
        updatePdfSelectionPage(runtime, page, meta, textGeometry);
        schedulePdfSelectionIndexRebuild(runtime);
      } catch (geometryError) {
        addDebugEntry('warn', 'PDF glyph geometry failed', { page: pageNumber, error: geometryError });
      }
    }
    if (runtime.disposed || runtime.layoutSeq !== layoutSeq) return;
    recordPdfPerfSample({
      page: pageNumber,
      renderMs,
      geometryMs: Math.round(geometryMs),
      geometryMode
    });
  };

  const handlePdfPointerMove = (event) => {
    const drag = pdfDragRef.current;
    const shell = shellRef.current;
    if (!drag || drag.pointerId !== event.pointerId || !shell) return;
    const current = pdfPagePointFromEvent(event, shell);
    if (!current) return;
    const distance = Math.abs(current.clientX - drag.start.clientX) + Math.abs(current.clientY - drag.start.clientY);
    if (distance < 3 && !drag.moved) return;
    drag.moved = true;
    drag.current = current;
    event.preventDefault();
    if (!pdfSelectionFrameRef.current) {
      pdfSelectionFrameRef.current = requestAnimationFrame(() => {
        pdfSelectionFrameRef.current = 0;
        const activeDrag = pdfDragRef.current;
        if (activeDrag?.current) {
          applyPdfDragSelection(activeDrag, activeDrag.current);
        }
      });
    }
  };

  const handlePdfPointerUp = (event) => {
    const drag = pdfDragRef.current;
    const shell = shellRef.current;
    if (!drag || drag.pointerId !== event.pointerId || !shell) return;
    const current = pdfPagePointFromEvent(event, shell);
    pdfDragRef.current = null;
    event.currentTarget.releasePointerCapture?.(event.pointerId);
    if (!current || !drag.moved) {
      if (pdfSelectionFinalizeTimerRef.current) {
        cancelIdleCallbackSafe(pdfSelectionFinalizeTimerRef.current);
        pdfSelectionFinalizeTimerRef.current = 0;
      }
      if (showPdfSelectionDebug && current) {
        event.preventDefault();
        inspectPdfSelectionDebugAtPoint(current);
      } else {
        pdfSelectionRef.current = null;
        setPdfSelectionRects([]);
        setPdfSelectionDebugSnapshot(null);
        setPdfSelectionDebugDetail(null);
      }
      return;
    }
    event.preventDefault();
    if (pdfSelectionFrameRef.current) {
      cancelAnimationFrame(pdfSelectionFrameRef.current);
      pdfSelectionFrameRef.current = 0;
    }
    applyPdfDragSelection(drag, current);
    schedulePdfSelectionFinalize(drag, current);
  };

  const handlePdfPointerCancel = (event) => {
    if (pdfDragRef.current?.pointerId === event.pointerId) {
      pdfDragRef.current = null;
    }
    if (pdfSelectionFrameRef.current) {
      cancelAnimationFrame(pdfSelectionFrameRef.current);
      pdfSelectionFrameRef.current = 0;
    }
  };

  const scheduleVisiblePdfRender = () => {
    if (pdfRenderScheduleRef.current) return;
    pdfRenderScheduleRef.current = requestAnimationFrame(() => {
      pdfRenderScheduleRef.current = 0;
      renderVisiblePdfPages();
    });
  };

  const renderVisiblePdfPages = () => {
    const runtime = pdfRuntimeRef.current;
    const shell = shellRef.current;
    if (!runtime || runtime.disposed || !shell) return;
    const pageNodes = shell.querySelectorAll('.pdfium-page');
    if (pageNodes.length === 0) return;
    const shellRect = shell.getBoundingClientRect();
    const prefetch = Math.max(600, shell.clientHeight * 1.4);
    const viewportCenter = shellRect.top + shellRect.height / 2;
    const visible = [];
    for (const node of pageNodes) {
      const rect = node.getBoundingClientRect();
      if (rect.bottom < shellRect.top - prefetch || rect.top > shellRect.bottom + prefetch) continue;
      const pageNumber = Number(node.dataset?.pdfPage || 0);
      if (pageNumber) {
        visible.push({
          pageNumber,
          score: Math.abs((rect.top + rect.bottom) / 2 - viewportCenter)
        });
      }
    }
    if (visible.length === 0 && renderState.pages.length > 0) {
      visible.push({ pageNumber: renderState.pages[0].pageNumber, score: 0 });
    }
    visible.sort((a, b) => a.score - b.score);
    for (const { pageNumber } of visible) {
      const meta = runtime.pageMetaByNumber.get(pageNumber);
      if (!meta || (meta.rendered && !meta.needsRerender)) continue;
      if (meta.needsRerender && performance.now() < (runtime.deferRenderUntil || 0)) continue;
      enqueuePdfPageRender(runtime, pageNumber);
    }
  };

  const enqueuePdfPageRender = (runtime, pageNumber) => {
    if (!runtime || runtime.disposed || runtime.renderQueued?.has(pageNumber)) return;
    runtime.renderQueued.add(pageNumber);
    runtime.renderQueue.push(pageNumber);
    schedulePdfRenderPump(runtime);
  };

  const schedulePdfRenderPump = (runtime, delay = 0) => {
    if (!runtime || runtime.disposed || runtime.renderPumpTimer) return;
    const timerId = window.setTimeout(() => {
      runtime.renderPumpTimer = 0;
      if (pdfRenderPumpTimerRef.current === timerId) {
        pdfRenderPumpTimerRef.current = 0;
      }
      pumpPdfRenderQueue(runtime);
    }, delay);
    runtime.renderPumpTimer = timerId;
    pdfRenderPumpTimerRef.current = timerId;
  };

  const pumpPdfRenderQueue = (runtime) => {
    if (!runtime || runtime.disposed) return;
    const workerCount = runtime.renderWorkers?.length || 1;
    const limit = Math.max(1, Math.min(PDF_RENDER_CONCURRENCY, workerCount));
    while (runtime.renderQueue.length > 0 && runtime.rendering.size < limit) {
      const pageNumber = runtime.renderQueue.shift();
      if (!pageNumber) continue;
      runtime.renderQueued.delete(pageNumber);
      renderPdfPageLazy(runtime, pageNumber).finally(() => {
        if (!runtime.disposed && runtime.renderQueue.length > 0) {
          schedulePdfRenderPump(runtime);
        }
      });
    }
  };

  const renderPdfPageLazy = async (runtime, pageNumber) => {
    if (!runtime || runtime.disposed || runtime.rendering.has(pageNumber)) return;
    const meta = runtime.pageMetaByNumber.get(pageNumber);
    const page = runtime.pageByNumber.get(pageNumber);
    if (!meta || !page || (meta.rendered && !meta.needsRerender)) return;
    const layoutSeq = runtime.layoutSeq || 0;
    const rerenderingForResize = Boolean(meta.needsRerender && meta.imageKey);
    const renderWorker = acquirePdfRenderWorker(runtime);
    const renderPage = renderWorker?.pageByNumber?.get(pageNumber) || page;
    runtime.rendering.add(pageNumber);
    if (renderWorker) renderWorker.inFlight = (renderWorker.inFlight || 0) + 1;
    if (!meta.imageKey) {
      schedulePdfPageMetaCommit(runtime, pageNumber, { ...meta, rendering: true });
    }
    try {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      const pageStartAt = performance.now();
      const image = await taskToPromise(
        (renderWorker?.engine || runtime.engine).renderPageRaw(renderWorker?.doc || runtime.doc, renderPage, {
          scaleFactor: meta.scaleFactor,
          dpr,
          withAnnotations: true,
          withForms: true,
          preferImageBitmap: true
        }),
        `renderPage:${pageNumber}`,
        30_000
      );
      const renderDoneAt = performance.now();
      if (runtime.disposed || runtime.layoutSeq !== layoutSeq) return;
      runtime.imageSeq = (runtime.imageSeq || 0) + 1;
      closePdfImage(runtime.pageImageByNumber.get(pageNumber));
      runtime.pageImageByNumber.set(pageNumber, image);
      const imageKey = `${runtime.revision}:${pageNumber}:${runtime.imageSeq}`;
      const pixels = image?.width && image?.height
        ? `${image.width}x${image.height}${image.bitmapMode ? ` ${image.bitmapMode}` : ''}`
        : '';
      const renderMs = Math.round(renderDoneAt - pageStartAt);
      const visualMeta = {
        ...meta,
        imageKey,
        textRuns: meta.textRuns || [],
        rendered: true,
        rendering: false,
        needsRerender: false
      };
      schedulePdfPageMetaCommit(runtime, pageNumber, visualMeta);
      recordPdfPerfSample({
        page: pageNumber,
        renderMs,
        totalMs: renderMs,
        geometryMode: 'pending',
        pixels
      });
      schedulePdfTextGeometry(runtime, pageNumber, page, visualMeta, layoutSeq, renderMs);
    } catch (error) {
      addDebugEntry('warn', 'Visible PDF page render failed', { page: pageNumber, error });
      schedulePdfPageMetaCommit(runtime, pageNumber, {
        ...(runtime.pageMetaByNumber.get(pageNumber) || meta),
        rendering: false,
        error: error?.message || '页面渲染失败'
      });
    } finally {
      if (renderWorker) renderWorker.inFlight = Math.max(0, (renderWorker.inFlight || 1) - 1);
      runtime.rendering.delete(pageNumber);
    }
  };

  const refreshPdf = () => {
    const node = shellRef.current;
    const maxScrollTop = node ? Math.max(0, node.scrollHeight - node.clientHeight) : 0;
    onRefreshPdfPreview?.(file, {
      scroll_top: node?.scrollTop || 0,
      scroll_left: node?.scrollLeft || 0,
      ratio_top: maxScrollTop > 0 ? (node.scrollTop / maxScrollTop) : 0,
      updated_at: Date.now()
    });
  };

  return (
    <div className={`pdf-preview${showInfoPanel ? ' showing-debug' : ''}`}>
      <div className="pdf-preview-toolbar">
        <div className="pdf-preview-title">
          <strong>{name}</strong>
          {file.preview_size ? <span>{Math.ceil(file.preview_size / 1024)} KB</span> : null}
        </div>
        <div className="pdf-preview-actions">
          <button
            className="secondary-button icon-only"
            type="button"
            title={showInfoPanel ? '隐藏 PDF 信息' : '显示 PDF 信息'}
            aria-label={showInfoPanel ? '隐藏 PDF 信息' : '显示 PDF 信息'}
            aria-pressed={showInfoPanel}
            onClick={() => setShowInfoPanel((value) => !value)}
          >
            <Info size={14} />
          </button>
          <button
            className={`secondary-button icon-only${showPdfSelectionDebug ? ' active' : ''}`}
            type="button"
            title={showPdfSelectionDebug ? '隐藏选区调试层' : '显示选区调试层'}
            aria-label={showPdfSelectionDebug ? '隐藏选区调试层' : '显示选区调试层'}
            aria-pressed={showPdfSelectionDebug}
            onClick={togglePdfSelectionDebug}
          >
            <Bug size={14} />
          </button>
          <button
            className="secondary-button icon-only"
            type="button"
            title="刷新 PDF"
            aria-label="刷新 PDF"
            onClick={refreshPdf}
            disabled={!onRefreshPdfPreview}
          >
            <RefreshCw size={14} />
          </button>
          <button
            className="secondary-button"
            type="button"
            onClick={printPdf}
          >
            <Printer size={14} />
            打印
          </button>
          <button
            className="secondary-button"
            type="button"
            onClick={() => onDownloadFile?.(file)}
          >
            <Download size={14} />
            下载
          </button>
        </div>
      </div>
      <div
        className="embedpdf-shell"
        ref={shellRef}
        onContextMenu={handleContextMenu}
        onPointerDown={handlePdfPointerDown}
        onPointerMove={handlePdfPointerMove}
        onPointerUp={handlePdfPointerUp}
        onPointerCancel={handlePdfPointerCancel}
      >
        {wasmError || renderState.error ? (
          <div className="panel-placeholder">{wasmError || renderState.error}</div>
        ) : (
          <div className="pdfium-pages">
            {renderState.pages.map((page) => (
              <div
                className={`pdfium-page${page.rendering ? ' rendering' : ''}${page.error ? ' error' : ''}`}
                data-pdf-page={page.pageNumber}
                key={`${documentId}-${page.pageNumber}`}
                style={{ width: `${page.width}px`, height: `${page.height}px` }}
              >
                {page.imageKey ? (
                  <PdfiumPageCanvas
                    image={pdfRuntimeRef.current?.pageImageByNumber?.get(page.pageNumber)}
                    imageKey={page.imageKey}
                    label={`${name} page ${page.pageNumber}`}
                    pageNumber={page.pageNumber}
                    onPaint={handlePdfCanvasPaint}
                  />
                ) : (
                  <div className="pdfium-page-placeholder">
                    <span>{page.error || (page.rendering ? '渲染中...' : '等待进入视口')}</span>
                  </div>
                )}
                <div className="pdfium-text-layer" data-pdf-page={page.pageNumber}>
                  {page.textRuns.map((run, index) => (
                    <span
                      key={`${page.pageNumber}-${index}`}
                      style={{
                        left: `${run.left}px`,
                        top: `${run.top}px`,
                        width: `${run.width}px`,
                        height: `${run.height}px`,
                        fontSize: `${run.fontSize}px`,
                        lineHeight: `${run.height}px`,
                        fontFamily: run.fontFamily,
                        transform: run.scaleX ? `scaleX(${run.scaleX})` : undefined,
                        transformOrigin: 'left top'
                      }}
                    >
                      {run.text}
                    </span>
                  ))}
                </div>
                <div className="pdfium-selection-overlay" aria-hidden="true">
                  {(pdfSelectionRectsByPage.get(page.pageNumber) || []).map((rect, index) => (
                    <span
                      key={`${page.pageNumber}-${index}-${rect.left}-${rect.top}`}
                      style={{
                        left: `${rect.left}px`,
                        top: `${rect.top}px`,
                        width: `${rect.width}px`,
                        height: `${rect.height}px`
                      }}
                    />
                  ))}
                </div>
                {showPdfSelectionDebug ? (
                  <PdfSelectionDebugOverlay
                    debug={pdfSelectionDebugByPage.get(page.pageNumber)}
                    detail={pdfSelectionDebugDetail?.pageNumber === page.pageNumber ? pdfSelectionDebugDetail : null}
                  />
                ) : null}
                <span className="pdfium-page-number">{page.pageNumber}</span>
              </div>
            ))}
          </div>
        )}
      </div>
      {renderState.loading && !showInfoPanel ? (
        <div className="pdf-loading-status">
          PDFium 渲染中{renderState.pageCount ? ` ${pdfPerfStats.renderedPages}/${renderState.pageCount}` : ''}
        </div>
      ) : null}
      {showInfoPanel ? (
        <div className="pdf-debug-overlay">
          <strong>{renderState.loading ? `PDFium 渲染中${renderState.pageCount ? `：${pdfPerfStats.renderedPages}/${renderState.pageCount}` : ''}` : 'PDF 信息'}</strong>
          <span>当前引擎：{activePdfEngineLabel}</span>
          <div className="pdf-perf-grid">
            <span><b>{pdfPerfStats.renderedPages}/{renderState.pageCount || 0}</b><small>已渲染页</small></span>
            <span><b>{averagePdfMs(pdfPerfStats.totalMsTotal, pdfPerfStats.renderedPages)}</b><small>平均总耗时</small></span>
            <span><b>{averagePdfMs(pdfPerfStats.renderMsTotal, pdfPerfStats.renderedPages)}</b><small>平均渲染</small></span>
            <span><b>{averagePdfMs(pdfPerfStats.geometryMsTotal, pdfPerfStats.renderedPages)}</b><small>平均文字几何</small></span>
            <span><b>{averagePdfMs(pdfPerfStats.paintMsTotal, pdfPerfStats.paintedPages)}</b><small>平均画布绘制</small></span>
            <span><b>{formatPdfMs(pdfPerfStats.maxTotalMs)}</b><small>单页最慢</small></span>
          </div>
          <span>最近页面：{pdfPerfStats.lastPage ? `第 ${pdfPerfStats.lastPage} 页` : '-'}{pdfPerfStats.lastPixels ? `，${pdfPerfStats.lastPixels}` : ''}</span>
          {pdfSelectionDebugDetail ? (
            <pre>{stringifyDebugDetail(pdfSelectionDebugDetail.detail)}</pre>
          ) : null}
          {pdfSelectionDebugSnapshot ? (
            <pre>{stringifyDebugDetail(pdfSelectionDebugSnapshot.summary)}</pre>
          ) : null}
          <pre>{debugEntries.map((entry) => (
            `[${entry.at}] ${entry.level.toUpperCase()} ${entry.message}${entry.detail ? `\n${entry.detail}` : ''}`
          )).join('\n\n')}</pre>
        </div>
      ) : null}
    </div>
  );
}

function pdfEmbedUrl(url, zoomPercent = 100) {
  return `${url}#toolbar=0&navpanes=0&scrollbar=1&view=FitH&zoom=${Math.round(zoomPercent)}`;
}

function MarkdownBlock({ text, path, onResolveAsset }) {
  const parsed = useMemo(() => splitMarkdownMetadata(text), [text]);
  const components = useMemo(() => ({
    a: ({ node, ...props }) => (
      <a
        {...props}
        target={isExternalUrl(props.href) ? '_blank' : undefined}
        rel={isExternalUrl(props.href) ? 'noreferrer' : undefined}
        onClick={(event) => handleExternalLinkClick(event, props.href)}
      />
    ),
    img: (props) => (
      <MarkdownImage
        {...props}
        markdownPath={path}
        onResolveAsset={onResolveAsset}
      />
    )
  }), [path, onResolveAsset]);
  return (
    <>
      {parsed.metadata.length ? <MarkdownMetadata entries={parsed.metadata} /> : null}
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeRaw, rehypeHighlight]}
        components={components}
      >
        {parsed.body}
      </ReactMarkdown>
    </>
  );
}

function MarkdownImage({ markdownPath, onResolveAsset, src, alt, ...props }) {
  const [resolvedSrc, setResolvedSrc] = useState(src || '');

  useEffect(() => {
    let disposed = false;
    const source = String(src || '');
    setResolvedSrc(source);
    if (!source || !onResolveAsset) return () => {
      disposed = true;
    };
    onResolveAsset(markdownPath, source)
      .then((value) => {
        if (!disposed && value) setResolvedSrc(value);
      })
      .catch(() => {});
    return () => {
      disposed = true;
    };
  }, [markdownPath, onResolveAsset, src]);

  return (
    <img
      {...props}
      src={resolvedSrc}
      alt={alt || ''}
      loading="lazy"
      referrerPolicy="no-referrer"
    />
  );
}

function MarkdownMetadata({ entries }) {
  return (
    <section className="markdown-metadata" aria-label="Markdown metadata">
      {entries.map((entry) => (
        <div className="markdown-metadata-row" key={entry.key}>
          <span>{entry.key}</span>
          <strong>{entry.value}</strong>
        </div>
      ))}
    </section>
  );
}

function splitMarkdownMetadata(text) {
  const source = String(text || '').replace(/^\uFEFF/, '');
  const delimiter = source.startsWith('---\n') || source.startsWith('---\r\n')
    ? '---'
    : source.startsWith('+++\n') || source.startsWith('+++\r\n')
      ? '+++'
      : '';
  if (!delimiter) return { metadata: [], body: source };
  const lines = source.split(/\r?\n/);
  const endIndex = lines.findIndex((line, index) => index > 0 && line.trim() === delimiter);
  if (endIndex <= 1 || endIndex > 80) return { metadata: [], body: source };
  const rawMetadata = lines.slice(1, endIndex);
  const metadata = parseFrontMatterLines(rawMetadata, delimiter).slice(0, 24);
  if (!metadata.length) return { metadata: [], body: source };
  return {
    metadata,
    body: lines.slice(endIndex + 1).join('\n').replace(/^\s*\n/, '')
  };
}

function parseFrontMatterLines(lines, delimiter) {
  const entries = [];
  let current = null;
  for (const rawLine of lines) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;
    const match = delimiter === '+++'
      ? line.match(/^([A-Za-z0-9_.-]+)\s*=\s*(.+)$/)
      : line.match(/^([A-Za-z0-9_.-]+)\s*:\s*(.*)$/);
    if (match) {
      current = {
        key: match[1],
        value: cleanMetadataValue(match[2])
      };
      entries.push(current);
      continue;
    }
    if (current && /^-\s+/.test(line)) {
      const item = cleanMetadataValue(line.replace(/^-\s+/, ''));
      current.value = current.value ? `${current.value}, ${item}` : item;
    }
  }
  return entries.filter((entry) => entry.key && entry.value);
}

function cleanMetadataValue(value) {
  return String(value || '')
    .trim()
    .replace(/^\[(.*)]$/, '$1')
    .replace(/^["']|["']$/g, '')
    .replace(/\s+/g, ' ');
}

function WordPreview({ file, name, source, onSelectionContextMenu }) {
  const articleRef = useRef(null);
  const previewRef = useRef(null);
  const docxScaleRef = useRef(1);
  const [text, setText] = useState(String(source || '').trim());
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [docxScale, setDocxScale] = useState(1);
  const [layoutVersion, setLayoutVersion] = useState(0);

  useLayoutEffect(() => {
    const article = articleRef.current;
    const container = previewRef.current;
    if (!article || !container) return undefined;

    let animationFrame = 0;
    const updateScale = () => {
      if (animationFrame) cancelAnimationFrame(animationFrame);
      animationFrame = requestAnimationFrame(() => {
        const page = container.querySelector('.docx-wrapper > section, section.docx, section.docx-preview-document');
        if (!page) {
          docxScaleRef.current = 1;
          setDocxScale(1);
          return;
        }

        const style = window.getComputedStyle(article);
        const horizontalPadding = parseFloat(style.paddingLeft || '0') + parseFloat(style.paddingRight || '0');
        const availableWidth = Math.max(1, article.clientWidth - horizontalPadding);
        const currentScale = docxScaleRef.current || 1;
        const measuredWidth = page.getBoundingClientRect().width || page.scrollWidth || page.clientWidth || 1;
        const unscaledWidth = measuredWidth / currentScale;
        const nextScale = Math.min(1, Math.max(0.35, availableWidth / Math.max(1, unscaledWidth)));

        if (Math.abs(nextScale - currentScale) > 0.005) {
          docxScaleRef.current = nextScale;
          setDocxScale(nextScale);
        }
      });
    };

    updateScale();
    const resizeObserver = new ResizeObserver(updateScale);
    const mutationObserver = new MutationObserver(updateScale);
    resizeObserver.observe(article);
    mutationObserver.observe(container, { childList: true, subtree: true });

    return () => {
      if (animationFrame) cancelAnimationFrame(animationFrame);
      resizeObserver.disconnect();
      mutationObserver.disconnect();
    };
  }, [layoutVersion]);

  useEffect(() => {
    let disposed = false;
    const container = previewRef.current;
    if (container) container.replaceChildren();
    docxScaleRef.current = 1;
    setDocxScale(1);
    setLayoutVersion((version) => version + 1);
    setText(String(source || '').trim());
    setLoading(false);
    setError('');
    const base64 = file?.encoding === 'base64' ? String(file.data || file.content || '') : '';
    if (fileExtension(name) === 'doc') {
      setError('旧版 .doc 当前无法保留 Word 版式预览，请转换为 .docx 后查看。');
      return () => {
        disposed = true;
      };
    }
    if (!base64 || fileExtension(name) !== 'docx') return () => {
      disposed = true;
    };
    const arrayBuffer = base64ToArrayBuffer(base64);
    setLoading(true);
    Promise.allSettled([
      container
        ? renderDocxAsync(arrayBuffer.slice(0), container, undefined, {
          inWrapper: true,
          breakPages: true,
          ignoreLastRenderedPageBreak: false,
          ignoreWidth: false,
          ignoreHeight: false,
          ignoreFonts: false,
          renderHeaders: true,
          renderFooters: true,
          renderFootnotes: true,
          renderEndnotes: true,
          useBase64URL: true,
          experimental: true
        })
        : Promise.resolve(),
      mammoth.extractRawText({ arrayBuffer: arrayBuffer.slice(0) })
    ])
      .then(([renderResult, textResult]) => {
        if (disposed) return;
        if (renderResult.status === 'rejected') {
          setError(renderResult.reason?.message || 'Word 文档预览失败');
        }
        if (textResult.status === 'fulfilled') {
          setText(textResult.value?.value || '');
        }
        setLayoutVersion((version) => version + 1);
      })
      .catch((err) => {
        if (!disposed) setError(err?.message || 'Word 文档预览失败');
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, [file, name, source]);

  if (error && !loading && !text && !previewRef.current?.childElementCount) {
    return (
      <div className="file-binary-preview">
        <FileText size={34} />
        <strong>{name}</strong>
        <span>{error}</span>
      </div>
    );
  }
  return (
    <article
      ref={articleRef}
      className="word-preview"
      data-selection-kind="docx"
      onContextMenu={(event) => handleDomSelectionContextMenu(event, event.currentTarget, file, 'docx', text || event.currentTarget.textContent || '', onSelectionContextMenu)}
    >
      {loading ? <div className="word-preview-status">正在渲染 Word 版式...</div> : null}
      {error ? <div className="word-preview-status error">{error}</div> : null}
      <div className="word-preview-pages" ref={previewRef} style={{ '--docx-scale': docxScale }} />
    </article>
  );
}

function CodePreview({ file, code, language, sourceKind = 'code', onSelectionContextMenu }) {
  const highlighted = highlightCode(code, language);
  return (
    <pre
      className="file-code-view"
      data-selection-kind={sourceKind}
      onContextMenu={(event) => handleCodeSelectionContextMenu(event, event.currentTarget, file, sourceKind, code, onSelectionContextMenu)}
    >
      <code className="hljs" dangerouslySetInnerHTML={{ __html: highlighted }} />
    </pre>
  );
}

function buildCodeSelectionReference(container, file, sourceKind, sourceText) {
  const selection = selectionWithin(container);
  if (!selection) return null;
  const locator = lineLocatorForOffset(String(sourceText || ''), selection.startOffset, selection.text.length);
  return buildSelectionReference({
    file,
    sourceKind,
    selectedText: selection.text,
    sourceText,
    startOffset: selection.startOffset,
    locator: {
      kind: 'line_range',
      ...locator
    }
  });
}

function buildDomSelectionReference(container, file, sourceKind, sourceText) {
  const selection = selectionWithin(container);
  if (!selection) return null;
  const heading = nearestSelectionHeading(container, selection.range);
  return buildSelectionReference({
    file,
    sourceKind,
    selectedText: selection.text,
    sourceText: container.textContent || sourceText || '',
    startOffset: selection.startOffset,
    locator: {
      kind: sourceKind === 'markdown' ? 'rendered_markdown_text' : 'rendered_text',
      heading,
      text_offset: selection.startOffset,
      text_length: selection.text.length
    }
  });
}

async function buildEmbedPdfSelectionReference(selectionScope, file, documentId) {
  if (!selectionScope?.getSelectedText) return null;
  const selectedTextParts = await taskToPromise(selectionScope.getSelectedText()).catch(() => []);
  const selectedText = Array.isArray(selectedTextParts)
    ? selectedTextParts.join('\n').trim()
    : String(selectedTextParts || '').trim();
  if (!selectedText) return null;
  const formatted = selectionScope.getFormattedSelection?.() || [];
  const locator = {
    kind: 'pdf_selection',
    document_id: documentId,
    ...locatorFromEmbedPdfSelection(formatted)
  };
  return buildSelectionReference({
    file,
    sourceKind: 'pdf',
    selectedText,
    sourceText: selectedText,
    startOffset: 0,
    locator
  });
}

function buildPdfSelectionReference(container, file, customSelection = null) {
  if (customSelection?.selectedText) {
    return buildSelectionReference({
      file,
      sourceKind: 'pdf',
      selectedText: customSelection.selectedText,
      sourceText: customSelection.selectedText,
      startOffset: 0,
      locator: customSelection.locator || {
        kind: 'pdf_text_selection',
        page: customSelection.page,
        text_length: customSelection.selectedText.length
      }
    });
  }
  const selection = selectionWithin(container);
  if (!selection) return null;
  const pageNode = selection.range.commonAncestorContainer instanceof Element
    ? selection.range.commonAncestorContainer.closest?.('.pdfium-page')
    : selection.range.commonAncestorContainer?.parentElement?.closest?.('.pdfium-page');
  const page = Number(pageNode?.querySelector?.('.pdfium-text-layer')?.dataset?.pdfPage || 0) || undefined;
  return buildSelectionReference({
    file,
    sourceKind: 'pdf',
    selectedText: selection.text,
    sourceText: container?.textContent || selection.text,
    startOffset: selection.startOffset,
    locator: {
      kind: 'pdf_text_selection',
      page,
      text_offset: selection.startOffset,
      text_length: selection.text.length
    }
  });
}

function pdfPointFromEvent(event) {
  if (!event || !Number.isFinite(event.clientX) || !Number.isFinite(event.clientY)) return null;
  return {
    clientX: event.clientX,
    clientY: event.clientY
  };
}

function isPdfGlyphEngine(engine) {
  return engine === 'pdfium-glyph' || engine === 'pdfium-worker';
}

function pdfPagePointFromEvent(event, shell) {
  if (!event || !shell || !Number.isFinite(event.clientX) || !Number.isFinite(event.clientY)) return null;
  const hit = document.elementFromPoint(event.clientX, event.clientY);
  const hitPage = hit?.closest?.('.pdfium-page');
  if (hitPage && shell.contains(hitPage)) {
    return pdfPagePointFromPageElement(event, hitPage);
  }
  const pages = shell.querySelectorAll('.pdfium-page');
  if (pages.length === 0) return null;
  let best = null;
  for (const page of pages) {
    const rect = page.getBoundingClientRect();
    const verticalDistance = event.clientY < rect.top ? rect.top - event.clientY : event.clientY > rect.bottom ? event.clientY - rect.bottom : 0;
    const horizontalDistance = event.clientX < rect.left ? rect.left - event.clientX : event.clientX > rect.right ? event.clientX - rect.right : 0;
    const score = verticalDistance * 1000 + horizontalDistance;
    if (!best || score < best.score) best = { page, rect, score };
  }
  if (!best) return null;
  return pdfPagePointFromPageElement(event, best.page, best.rect);
}

function pdfPagePointFromPageElement(event, page, cachedRect = null) {
  const rect = cachedRect || page.getBoundingClientRect();
  const layer = page.querySelector('.pdfium-text-layer');
  const declaredWidth = parseFloat(page.style.width || '0') || rect.width || 1;
  const declaredHeight = parseFloat(page.style.height || '0') || rect.height || 1;
  const scaleX = rect.width / declaredWidth;
  const scaleY = rect.height / declaredHeight;
  return {
    pageNumber: Number(layer?.dataset?.pdfPage || 0) || undefined,
    x: clampNumber((event.clientX - rect.left) / Math.max(scaleX, 0.0001), 0, declaredWidth),
    y: clampNumber((event.clientY - rect.top) / Math.max(scaleY, 0.0001), 0, declaredHeight),
    clientX: event.clientX,
    clientY: event.clientY
  };
}

function buildPdfGlyphDragSelection(index, startPoint, endPoint, options = {}) {
  if (!index || !startPoint || !endPoint) return null;
  const anchor = index.hitTest(startPoint);
  const focus = index.hitTest(endPoint);
  const clientDeltaY = Math.abs((endPoint.clientY ?? endPoint.y ?? 0) - (startPoint.clientY ?? startPoint.y ?? 0));
  const pageDeltaY = Math.abs((endPoint.y ?? 0) - (startPoint.y ?? 0));
  const selection = index.selectBetween(anchor, focus, {
    sameLaneSelection: true,
    focusPoint: endPoint,
    forceAnchorLine: clientDeltaY <= 18 || pageDeltaY <= 18,
    includeText: options.includeText ?? true
  });
  if (!selection || selection.glyphCount <= 0) return null;
  if ((options.includeText ?? true) && !selection.selectedText) return null;
  const pages = selection.pages || [];
  const rects = mergePdfSelectionRects(selection.rects || []);
  if (rects.length === 0) return null;
  return {
    selectedText: selection.selectedText || '',
    textReady: Boolean(selection.textReady),
    rects,
    page: pages[0],
    debug: {
      anchor: summarizePdfSelectionHit(anchor),
      focus: summarizePdfSelectionHit(focus),
      start: selection.start,
      end: selection.end,
      glyphCount: selection.glyphCount,
      rawRectCount: selection.rects?.length || 0,
      mergedRectCount: rects.length
    },
    locator: selection.textReady ? {
        kind: 'pdf_glyph_selection',
        page: pages[0],
        pages,
        text_length: selection.selectedText.length,
        glyph_count: selection.glyphCount,
        rects: (selection.rects || []).slice(0, 64).map((rect) => ({
          page: rect.pageNumber,
          left: roundPdfNumber(rect.left),
          top: roundPdfNumber(rect.top),
          width: roundPdfNumber(rect.width),
          height: roundPdfNumber(rect.height)
        }))
      } : null
  };
}

function buildPdfSelectionDebugSnapshot(index, selection, meta = {}) {
  if (!index) return null;
  const selectedOrders = new Set();
  const start = Number(selection?.debug?.start);
  const end = Number(selection?.debug?.end);
  if (Number.isFinite(start) && Number.isFinite(end)) {
    for (let order = start; order < end; order += 1) selectedOrders.add(order);
  }

  const anchorHit = meta.anchorPoint ? summarizePdfSelectionHit(index.hitTest(meta.anchorPoint)) : selection?.debug?.anchor || null;
  const focusHit = meta.focusPoint ? summarizePdfSelectionHit(index.hitTest(meta.focusPoint)) : selection?.debug?.focus || null;
  const selectionRects = selection?.rects || [];
  const selectionPages = new Set(selectionRects.map((rect) => rect.pageNumber).filter(Boolean));
  const pages = [];

  for (const page of index.pageMap?.values?.() || []) {
    const pageGlyphs = index.pageGlyphs?.get(page.pageNumber) || [];
    const shouldInclude =
      selectionPages.size === 0 ||
      selectionPages.has(page.pageNumber) ||
      meta.anchorPoint?.pageNumber === page.pageNumber ||
      meta.focusPoint?.pageNumber === page.pageNumber;
    if (!shouldInclude) continue;

    const glyphs = pageGlyphs.map((glyph) => ({
      key: glyph.id || `${glyph.pageNumber}:${glyph.localIndex}`,
      char: debugGlyphLabel(glyph.char),
      order: glyph.order,
      localIndex: glyph.localIndex,
      lineId: glyph.line?.id ?? glyph.lineId ?? '',
      laneIndex: glyph.laneIndex ?? -1,
      blockId: glyph.block?.id ?? '',
      blockType: glyph.block?.type ?? '',
      selected: selectedOrders.has(glyph.order),
      rect: debugRect(glyph.rect),
      hitRect: debugRect(glyph.hitRect || glyph.tightRect || glyph.rect)
    })).filter((glyph) => glyph.rect);

    const lines = Array.from(index.lineGlyphs?.values?.() || [])
      .filter((line) => line.pageNumber === page.pageNumber)
      .map((line) => {
        const bounds = debugRectFromGlyphs(line.glyphs || []);
        return bounds ? {
          key: line.key,
          lineId: line.lineId ?? '',
          laneIndex: line.laneIndex ?? -1,
          blockId: line.blockId ?? '',
          blockType: line.blockType ?? '',
          tableLike: Boolean(line.tableLike),
          indexInLane: line.indexInLane,
          glyphCount: line.glyphs?.length || 0,
          text: debugLinePreview(line.glyphs || []),
          rect: bounds
        } : null;
      })
      .filter(Boolean);

    const lanes = (page.lanes || []).map((lane) => ({
      key: `${page.pageNumber}:${lane.laneIndex}`,
      laneIndex: lane.laneIndex,
      left: roundPdfNumber(lane.left),
      right: roundPdfNumber(lane.right),
      margin: roundPdfNumber(lane.margin || 0),
      rect: {
        left: roundPdfNumber(Math.max(0, lane.left - (lane.margin || 0))),
        top: 0,
        width: roundPdfNumber(Math.max(1, lane.right - lane.left + (lane.margin || 0) * 2)),
        height: roundPdfNumber(page.height || 0)
      }
    }));

    const blocks = (page.blocks || []).map((block) => ({
      key: `${page.pageNumber}:${block.id}`,
      id: block.id,
      type: block.type,
      laneIndex: block.laneIndex ?? -1,
      lineCount: block.units?.length || 0,
      rect: debugRect(block.bounds || block)
    })).filter((block) => block.rect);

    const points = [
      debugPoint('anchor', meta.anchorPoint),
      debugPoint('focus', meta.focusPoint)
    ].filter((point) => point && point.pageNumber === page.pageNumber);

    pages.push({
      pageNumber: page.pageNumber,
      width: roundPdfNumber(page.width || 0),
      height: roundPdfNumber(page.height || 0),
      glyphs,
      lines,
      lanes,
      blocks,
      selectionRects: selectionRects
        .filter((rect) => rect.pageNumber === page.pageNumber)
        .map((rect, rectIndex) => ({
          key: `${page.pageNumber}:${rectIndex}:${rect.left}:${rect.top}`,
          rect: debugRect(rect)
        }))
        .filter((entry) => entry.rect),
      points
    });
  }

  return {
    version: PDF_SELECTION_ENGINE_VERSION,
    phase: meta.phase || 'manual',
    summary: {
      phase: meta.phase || 'manual',
      engineVersion: index.engineVersion,
      pageCount: index.pages?.length || 0,
      glyphCount: index.glyphs?.length || 0,
      selectedGlyphRange: Number.isFinite(start) && Number.isFinite(end) ? `${start}..${end}` : '-',
      selectedGlyphCount: selection?.debug?.glyphCount || 0,
      rawSelectionRects: selection?.debug?.rawRectCount || 0,
      mergedSelectionRects: selection?.debug?.mergedRectCount || selectionRects.length || 0,
      anchor: anchorHit,
      focus: focusHit
    },
    pages
  };
}

function summarizePdfSelectionHit(hit) {
  if (!hit?.glyph) return null;
  const glyph = hit.glyph;
  return {
    page: hit.pageNumber,
    point: {
      x: roundPdfNumber(hit.x),
      y: roundPdfNumber(hit.y)
    },
    boundary: hit.boundary,
    glyph: {
      char: debugGlyphLabel(glyph.char),
      order: glyph.order,
      localIndex: glyph.localIndex,
      lineId: glyph.line?.id ?? glyph.lineId ?? '',
      laneIndex: glyph.laneIndex ?? -1,
      blockId: glyph.block?.id ?? '',
      blockType: glyph.block?.type ?? '',
      rect: debugRect(glyph.rect),
      hitRect: debugRect(glyph.hitRect || glyph.tightRect || glyph.rect)
    },
    lane: hit.lane ? {
      index: hit.lane.laneIndex,
      left: roundPdfNumber(hit.lane.left),
      right: roundPdfNumber(hit.lane.right),
      margin: roundPdfNumber(hit.lane.margin)
    } : null
  };
}

function debugRect(rect) {
  if (!rect) return null;
  const left = Number(rect.left);
  const top = Number(rect.top);
  const width = Number(rect.width);
  const height = Number(rect.height);
  if (!(width > 0) || !(height > 0) || !Number.isFinite(left) || !Number.isFinite(top)) return null;
  return {
    left: roundPdfNumber(left),
    top: roundPdfNumber(top),
    width: roundPdfNumber(width),
    height: roundPdfNumber(height)
  };
}

function debugRectFromGlyphs(glyphs) {
  let left = Infinity;
  let top = Infinity;
  let right = -Infinity;
  let bottom = -Infinity;
  let count = 0;
  for (const glyph of glyphs) {
    const rect = glyph?.hitRect || glyph?.rect;
    if (!rect) continue;
    left = Math.min(left, rect.left);
    top = Math.min(top, rect.top);
    right = Math.max(right, rect.right);
    bottom = Math.max(bottom, rect.bottom);
    count += 1;
  }
  if (count === 0) return null;
  return debugRect({ left, top, width: right - left, height: bottom - top });
}

function debugPoint(kind, point) {
  if (!point || !Number.isFinite(point.x) || !Number.isFinite(point.y)) return null;
  return {
    key: kind,
    kind,
    pageNumber: point.pageNumber,
    x: roundPdfNumber(point.x),
    y: roundPdfNumber(point.y)
  };
}

function debugGlyphLabel(char) {
  if (char === ' ') return 'space';
  if (char === '\n') return '\\n';
  return String(char || '').slice(0, 8);
}

function debugLinePreview(glyphs) {
  return glyphs
    .slice()
    .sort((a, b) => a.left - b.left || a.localIndex - b.localIndex)
    .map((glyph) => glyph.char || '')
    .join('')
    .trim()
    .slice(0, 80);
}

function findPdfSelectionDebugItem(snapshot, point) {
  const page = snapshot?.pages?.find((candidate) => candidate.pageNumber === point?.pageNumber);
  if (!page || !point) return null;
  const x = Number(point.x);
  const y = Number(point.y);
  if (!Number.isFinite(x) || !Number.isFinite(y)) return null;

  const selection = smallestContaining(page.selectionRects, x, y, (entry) => entry.rect);
  if (selection) {
    return debugItem('selection', page.pageNumber, selection.key, selection.rect, {
      kind: 'selection',
      page: page.pageNumber,
      rect: selection.rect
    });
  }

  const glyph = smallestContaining(page.glyphs, x, y, (entry) => entry.hitRect || entry.rect);
  if (glyph) {
    const line = page.lines.find((candidate) => (
      candidate.lineId === glyph.lineId &&
      candidate.laneIndex === glyph.laneIndex &&
      pointInsideDebugRect(x, y, candidate.rect)
    )) || smallestContaining(page.lines, x, y, (entry) => entry.rect);
    return debugItem('glyph', page.pageNumber, glyph.key, glyph.hitRect || glyph.rect, {
      kind: 'glyph',
      page: page.pageNumber,
      char: glyph.char,
      order: glyph.order,
      localIndex: glyph.localIndex,
      laneIndex: glyph.laneIndex,
      blockId: glyph.blockId,
      blockType: glyph.blockType,
      lineId: glyph.lineId,
      selected: glyph.selected,
      rect: glyph.rect,
      hitRect: glyph.hitRect,
      line: line ? debugLineDetail(line) : null
    });
  }

  const block = smallestContaining(page.blocks || [], x, y, (entry) => entry.rect);
  if (block) {
    return debugItem('block', page.pageNumber, block.key, block.rect, {
      kind: 'block',
      page: page.pageNumber,
      blockId: block.id,
      blockType: block.type,
      laneIndex: block.laneIndex,
      lineCount: block.lineCount,
      rect: block.rect
    });
  }

  const line = smallestContaining(page.lines, x, y, (entry) => entry.rect);
  if (line) {
    return debugItem('line', page.pageNumber, line.key, line.rect, debugLineDetail(line));
  }

  const lane = smallestContaining(page.lanes, x, y, (entry) => entry.rect);
  if (lane) {
    return debugItem('lane', page.pageNumber, lane.key, lane.rect, {
      kind: 'lane',
      page: page.pageNumber,
      laneIndex: lane.laneIndex,
      left: lane.left,
      right: lane.right,
      margin: lane.margin,
      rect: lane.rect
    });
  }

  return null;
}

function debugLineDetail(line) {
  return {
    kind: 'line',
    lineId: line.lineId,
    laneIndex: line.laneIndex,
    blockId: line.blockId,
    blockType: line.blockType,
    tableLike: Boolean(line.tableLike),
    row: line.indexInLane,
    glyphCount: line.glyphCount,
    rect: line.rect,
    text: line.text
  };
}

function debugItem(kind, pageNumber, key, rect, detail) {
  return {
    kind,
    pageNumber,
    key,
    rect,
    detail
  };
}

function smallestContaining(items, x, y, rectForItem) {
  let best = null;
  for (const item of items || []) {
    const rect = rectForItem(item);
    if (!pointInsideDebugRect(x, y, rect)) continue;
    const area = Math.max(1, rect.width * rect.height);
    if (!best || area < best.area) best = { item, area };
  }
  return best?.item || null;
}

function pointInsideDebugRect(x, y, rect) {
  if (!rect) return false;
  return x >= rect.left && x <= rect.left + rect.width && y >= rect.top && y <= rect.top + rect.height;
}

function buildPdfDragSelection(container, startPoint, endPoint, cachedRuns = null) {
  if (!container || !startPoint || !endPoint) return null;
  const runs = cachedRuns || collectPdfTextRuns(container).filter(isSelectablePdfTextRun);
  if (runs.length === 0) return null;
  const anchor = locatePdfTextPosition(runs, startPoint);
  const focus = locatePdfTextPosition(runs, endPoint);
  if (!anchor || !focus) return null;
  const lane = buildPdfSelectionLane(runs, anchor, focus);
  const effectiveFocus = lane ? constrainPdfPositionToLane(runs, focus, endPoint, lane) : focus;
  const selectedRuns = pdfSelectedRunsFromRange(runs, anchor, effectiveFocus, lane);
  if (selectedRuns.length === 0) return null;
  const rects = mergePdfSelectionRects(selectedRuns.map((run) => run.rect));
  const selectedText = buildPdfSelectedText(selectedRuns);
  if (!selectedText) return null;
  const pages = Array.from(new Set(selectedRuns.map((run) => run.pageNumber).filter(Boolean)));
  return {
    selectedText,
    rects,
    page: pages[0],
    locator: {
      kind: 'pdf_text_run_selection',
      page: pages[0],
      pages,
      text_length: selectedText.length,
      rects: rects.slice(0, 64).map((rect) => ({
        page: rect.pageNumber,
        left: roundPdfNumber(rect.left),
        top: roundPdfNumber(rect.top),
        width: roundPdfNumber(rect.width),
        height: roundPdfNumber(rect.height)
      }))
    }
  };
}

function collectPdfTextRuns(container) {
  const spans = Array.from(container.querySelectorAll('.pdfium-text-layer span'));
  const runs = spans
    .map((span, index) => {
      const text = span.textContent || '';
      if (!text) return null;
      const pageNode = span.closest('.pdfium-page');
      const layer = span.closest('.pdfium-text-layer');
      if (!pageNode || !layer) return null;
      const pageRect = pageNode.getBoundingClientRect();
      const declaredWidth = parseFloat(pageNode.style.width || '0') || pageRect.width || 1;
      const declaredHeight = parseFloat(pageNode.style.height || '0') || pageRect.height || 1;
      const scaleX = pageRect.width / declaredWidth;
      const scaleY = pageRect.height / declaredHeight;
      const left = parseFloat(span.style.left || '0') || 0;
      const top = parseFloat(span.style.top || '0') || 0;
      const width = parseFloat(span.style.width || '0') || 0;
      const height = parseFloat(span.style.height || '0') || 0;
      if (!(width > 0) || !(height > 0)) return null;
      return {
        index,
        span,
        text,
        pageNumber: Number(layer.dataset?.pdfPage || 0) || undefined,
        left,
        top,
        width,
        height,
        right: left + width,
        centerX: left + width / 2,
        viewLeft: pageRect.left + left * scaleX,
        viewTop: pageRect.top + top * scaleY,
        viewWidth: width * scaleX,
        viewHeight: height * scaleY,
        pageWidth: declaredWidth,
        pageHeight: declaredHeight
      };
    })
    .filter(Boolean);
  return assignPdfReadingOrder(runs);
}

function isSelectablePdfTextRun(run) {
  const text = String(run?.text || '').trim();
  if (!text) return false;
  const pageWidth = run.pageWidth || 0;
  if (pageWidth > 0 && /^\d{1,5}$/.test(text)) {
    if (run.right < pageWidth * 0.08 || run.left > pageWidth * 0.92) {
      return false;
    }
  }
  return true;
}

function assignPdfReadingOrder(runs) {
  const runsByPage = new Map();
  for (const run of runs) {
    const key = run.pageNumber || 0;
    if (!runsByPage.has(key)) runsByPage.set(key, []);
    runsByPage.get(key).push(run);
  }

  const ordered = [];
  for (const pageNumber of Array.from(runsByPage.keys()).sort((a, b) => a - b)) {
    const pageRuns = runsByPage.get(pageNumber);
    const segments = buildPdfLineSegments(pageRuns);
    const lanes = buildPdfReadingLanes(segments, pageRuns[0]?.pageWidth || 0);
    const gutter = detectPdfColumnGutter(pageRuns, segments, pageRuns[0]?.pageWidth || 0);
    const runLane = new Map();
    for (const lane of lanes) {
      for (const segment of lane.segments) {
        for (const run of segment.runs) {
          const laneIndex = gutter ? pdfLaneIndexFromGutter(run, gutter) : lane.index;
          runLane.set(run.index, {
            laneIndex,
            lineTop: segment.top,
            segmentLeft: segment.left
          });
        }
      }
    }
    ordered.push(...pageRuns.sort((a, b) => {
      const laneA = runLane.get(a.index) || { laneIndex: 0, lineTop: a.top, segmentLeft: a.left };
      const laneB = runLane.get(b.index) || { laneIndex: 0, lineTop: b.top, segmentLeft: b.left };
      if (laneA.laneIndex !== laneB.laneIndex) return laneA.laneIndex - laneB.laneIndex;
      if (Math.abs(laneA.lineTop - laneB.lineTop) > 1.5) return laneA.lineTop - laneB.lineTop;
      if (Math.abs(laneA.segmentLeft - laneB.segmentLeft) > 1.5) return laneA.segmentLeft - laneB.segmentLeft;
      return a.left - b.left;
    }).map((run) => ({
      ...run,
      laneIndex: runLane.get(run.index)?.laneIndex || 0
    })));
  }

  return ordered.map((run, order) => ({ ...run, order }));
}

function locatePdfTextPosition(runs, point) {
  let best = null;
  for (const run of runs) {
    const verticalPadding = Math.max(3, run.viewHeight * 0.55);
    const horizontalPadding = Math.max(6, run.viewHeight * 0.35);
    const left = run.viewLeft - horizontalPadding;
    const right = run.viewLeft + run.viewWidth + horizontalPadding;
    const top = run.viewTop - verticalPadding;
    const bottom = run.viewTop + run.viewHeight + verticalPadding;
    const dx = point.clientX < left ? left - point.clientX : point.clientX > right ? point.clientX - right : 0;
    const dy = point.clientY < top ? top - point.clientY : point.clientY > bottom ? point.clientY - bottom : 0;
    const score = dy * 1000 + dx;
    if (!best || score < best.score) {
      best = {
        run,
        score,
        offset: pdfCharOffsetForRun(run, point.clientX)
      };
    }
  }
  if (!best) return null;
  return {
    run: best.run,
    runOrder: best.run.order,
    offset: best.offset
  };
}

function pdfCharOffsetForRun(run, clientX) {
  const textLength = run.text.length;
  if (textLength === 0 || !(run.viewWidth > 0)) return 0;
  const ratio = clampNumber((clientX - run.viewLeft) / run.viewWidth, 0, 1);
  return Math.max(0, Math.min(textLength, Math.round(ratio * textLength)));
}

function buildPdfSelectionLane(runs, anchor, focus) {
  const anchorRun = anchor.run;
  const focusRun = focus.run;
  if (!anchorRun || !focusRun || anchorRun.pageNumber !== focusRun.pageNumber) return null;
  const pageRuns = runs.filter((run) => run.pageNumber === anchorRun.pageNumber);
  const lanes = buildPdfReadingLanes(buildPdfLineSegments(pageRuns), anchorRun.pageWidth || focusRun.pageWidth || 0);
  const sourceLane = lanes.find((candidate) => candidate.index === anchorRun.laneIndex) || null;
  const source = (sourceLane?.segments || buildPdfLineSegments(pageRuns))
    .filter((segment) => segment.width >= Math.max(20, segment.height * 2))
    .filter((segment) => {
      const syntheticRun = { ...anchorRun, centerX: segment.centerX, laneIndex: anchorRun.laneIndex };
      return !Number.isFinite(anchorRun.laneIndex) || syntheticRun.laneIndex === anchorRun.laneIndex;
    });
  if (source.length === 0) return null;
  const left = Math.min(...source.map((segment) => segment.left));
  const right = Math.max(...source.map((segment) => segment.right));
  return {
    pageNumber: anchorRun.pageNumber,
    laneIndex: anchorRun.laneIndex,
    left,
    right,
    centerX: (left + right) / 2,
    margin: Math.max(8, Math.min(anchorRun.height, focusRun.height) * 1.2)
  };
}

function constrainPdfPositionToLane(runs, position, point, lane) {
  if (!lane || !position?.run || pdfRunMatchesSelectionLane(position.run, lane)) return position;
  const laneRuns = runs.filter((run) => pdfRunMatchesSelectionLane(run, lane));
  if (laneRuns.length === 0) return position;
  return locatePdfTextPosition(laneRuns, point) || position;
}

function buildPdfReadingLanes(segments, pageWidth) {
  const bodySegments = segments
    .filter((segment) => segment.width >= Math.max(30, segment.height * 3))
    .filter((segment) => !pageWidth || segment.width < pageWidth * 0.82);
  const clusters = [];
  for (const segment of bodySegments) {
    const center = segment.centerX;
    let cluster = clusters.find((candidate) => {
      const tolerance = Math.max(36, Math.min(pageWidth || 0, 900) * 0.12);
      return Math.abs(candidate.centerX - center) <= tolerance || rangesOverlapRatio(candidate, segment) >= 0.45;
    });
    if (!cluster) {
      cluster = {
        left: segment.left,
        right: segment.right,
        centerX: center,
        count: 0,
        segments: []
      };
      clusters.push(cluster);
    }
    cluster.segments.push(segment);
    cluster.count += 1;
    cluster.left = Math.min(cluster.left, segment.left);
    cluster.right = Math.max(cluster.right, segment.right);
    cluster.centerX = ((cluster.centerX * (cluster.count - 1)) + center) / cluster.count;
  }

  let lanes = clusters
    .filter((cluster) => cluster.count >= 2)
    .sort((a, b) => a.left - b.left);

  if (lanes.length === 0) {
    lanes = [{
      left: 0,
      right: pageWidth || Infinity,
      centerX: (pageWidth || 0) / 2,
      count: segments.length,
      segments: []
    }];
  }

  for (const segment of segments) {
    let best = null;
    for (const lane of lanes) {
      const centerDistance = Math.abs(segment.centerX - lane.centerX);
      const overlapBonus = rangesOverlapRatio(lane, segment) * Math.max(60, pageWidth * 0.12);
      const spanPenalty = pageWidth && segment.width > pageWidth * 0.82 ? 20 : 0;
      const score = centerDistance - overlapBonus + spanPenalty;
      if (!best || score < best.score) best = { lane, score };
    }
    best?.lane.segments.push(segment);
  }

  return lanes
    .sort((a, b) => a.left - b.left)
    .map((lane, index) => ({
      ...lane,
      index,
      segments: uniquePdfSegments(lane.segments).sort((a, b) => {
        if (Math.abs(a.top - b.top) > 1.5) return a.top - b.top;
        return a.left - b.left;
      })
    }));
}

function buildPdfLineSegments(pageRuns) {
  const rows = [];
  const sorted = [...pageRuns].sort((a, b) => {
    if (Math.abs(a.top - b.top) > 1.5) return a.top - b.top;
    return a.left - b.left;
  });
  for (const run of sorted) {
    const row = rows.find((candidate) => Math.abs(candidate.top - run.top) <= Math.max(2, Math.min(candidate.height, run.height) * 0.65));
    if (row) {
      row.runs.push(run);
      row.top = Math.min(row.top, run.top);
      row.height = Math.max(row.height, run.height);
    } else {
      rows.push({ top: run.top, height: run.height, runs: [run] });
    }
  }

  const segments = [];
  for (const row of rows) {
    const rowRuns = row.runs.sort((a, b) => a.left - b.left);
    let segmentRuns = [];
    let previous = null;
    for (const run of rowRuns) {
      const gap = previous ? run.left - previous.right : 0;
      const pageWidth = run.pageWidth || previous?.pageWidth || 0;
      const gapLimit = Math.max(7, Math.min(22, row.height * 1.15, pageWidth ? pageWidth * 0.035 : 22));
      if (previous && gap > gapLimit && segmentRuns.length > 0) {
        segments.push(pdfSegmentFromRuns(segmentRuns, row));
        segmentRuns = [];
      }
      segmentRuns.push(run);
      previous = run;
    }
    if (segmentRuns.length > 0) {
      segments.push(pdfSegmentFromRuns(segmentRuns, row));
    }
  }
  return segments.filter(Boolean);
}

function detectPdfColumnGutter(pageRuns, segments, pageWidth) {
  if (!(pageWidth > 0)) return null;
  const bodySegments = segments
    .filter((segment) => segment.width >= Math.max(35, segment.height * 3))
    .filter((segment) => segment.width < pageWidth * 0.72);
  if (bodySegments.length < 8) return null;
  const centers = bodySegments.map((segment) => segment.centerX).sort((a, b) => a - b);
  let bestGap = null;
  for (let index = 1; index < centers.length; index += 1) {
    const left = centers[index - 1];
    const right = centers[index];
    const gap = right - left;
    const midpoint = (left + right) / 2;
    if (midpoint < pageWidth * 0.28 || midpoint > pageWidth * 0.72) continue;
    const leftCount = centers.filter((center) => center < midpoint).length;
    const rightCount = centers.length - leftCount;
    if (leftCount < 3 || rightCount < 3) continue;
    if (!bestGap || gap > bestGap.gap) {
      bestGap = { gap, x: midpoint };
    }
  }
  if (!bestGap || bestGap.gap < Math.max(30, pageWidth * 0.06)) return null;

  const leftRuns = pageRuns.filter((run) => run.centerX < bestGap.x && run.width < pageWidth * 0.72);
  const rightRuns = pageRuns.filter((run) => run.centerX >= bestGap.x && run.width < pageWidth * 0.72);
  if (leftRuns.length < 8 || rightRuns.length < 8) return null;
  return bestGap.x;
}

function pdfLaneIndexFromGutter(run, gutterX) {
  return run.centerX < gutterX ? 0 : 1;
}

function rangesOverlapRatio(a, b) {
  const overlap = Math.max(0, Math.min(a.right, b.right) - Math.max(a.left, b.left));
  const width = Math.max(1, Math.min(a.right - a.left, b.right - b.left));
  return overlap / width;
}

function uniquePdfSegments(segments) {
  const seen = new Set();
  const unique = [];
  for (const segment of segments) {
    const key = segment.runs.map((run) => run.index).join(',');
    if (seen.has(key)) continue;
    seen.add(key);
    unique.push(segment);
  }
  return unique;
}

function pdfSegmentFromRuns(runs, row) {
  const left = Math.min(...runs.map((run) => run.left));
  const right = Math.max(...runs.map((run) => run.right));
  return {
    runs,
    top: row.top,
    height: row.height,
    left,
    right,
    width: right - left,
    centerX: (left + right) / 2
  };
}

function pdfLineSegmentForRun(pageRuns, targetRun) {
  return buildPdfLineSegments(pageRuns).find((segment) => segment.runs.some((run) => run.order === targetRun.order)) || null;
}

function pdfRunMatchesSelectionLane(run, lane) {
  if (!lane || run.pageNumber !== lane.pageNumber) return true;
  const left = lane.left - lane.margin;
  const right = lane.right + lane.margin;
  const center = run.centerX;
  return run.laneIndex === lane.laneIndex && center >= left && center <= right;
}

function pdfSelectedRunsFromRange(runs, anchor, focus, lane = null) {
  const forward = anchor.runOrder < focus.runOrder || (anchor.runOrder === focus.runOrder && anchor.offset <= focus.offset);
  const start = forward ? anchor : focus;
  const end = forward ? focus : anchor;
  const selected = [];
  for (const run of runs) {
    if (run.order < start.runOrder || run.order > end.runOrder) continue;
    if (!pdfRunMatchesSelectionLane(run, lane)) continue;
    const startOffset = run.order === start.runOrder ? start.offset : 0;
    const endOffset = run.order === end.runOrder ? end.offset : run.text.length;
    if (endOffset <= startOffset) continue;
    const text = run.text.slice(startOffset, endOffset);
    if (!text) continue;
    const rect = pdfSelectionRectForRun(run, startOffset, endOffset);
    if (!rect) continue;
    selected.push({
      ...run,
      selectedText: text,
      rect
    });
  }
  return selected;
}

function pdfSelectionRectForRun(run, startOffset, endOffset) {
  const text = run.text || '';
  if (!text || endOffset <= startOffset) return null;
  const fontSize = parseFloat(run.span.style.fontSize || '0') || run.height || 8;
  const fontFamily = run.span.style.fontFamily || 'serif';
  const totalWidth = measurePdfOverlayTextWidth(text, fontSize, fontFamily) || estimatePdfTextWidth(text, fontSize) || 1;
  const prefixWidth = measurePdfOverlayTextWidth(text.slice(0, startOffset), fontSize, fontFamily) || estimatePdfTextWidth(text.slice(0, startOffset), fontSize);
  const selectedWidth = measurePdfOverlayTextWidth(text.slice(startOffset, endOffset), fontSize, fontFamily) || estimatePdfTextWidth(text.slice(startOffset, endOffset), fontSize);
  return {
    pageNumber: run.pageNumber,
    left: run.left + (prefixWidth / totalWidth) * run.width,
    top: run.top,
    width: Math.max(1, (selectedWidth / totalWidth) * run.width),
    height: run.height
  };
}

function buildPdfSelectedText(selectedRuns) {
  const parts = [];
  let previous = null;
  for (const run of selectedRuns) {
    const text = run.selectedText;
    if (!text) continue;
    if (previous) {
      const newLine = previous.pageNumber !== run.pageNumber || Math.abs(previous.top - run.top) > Math.max(3, Math.min(previous.height, run.height) * 0.7);
      if (newLine) {
        parts.push('\n');
      } else if (needsPdfTextSpace(previous.selectedText, text)) {
        parts.push(' ');
      }
    }
    parts.push(text);
    previous = run;
  }
  return parts.join('').replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
}

function needsPdfTextSpace(left, right) {
  if (!left || !right) return false;
  const last = left.at(-1) || '';
  const first = right[0] || '';
  const noSpaceAfter = /\s/.test(last) || '([{/"\'“‘-'.includes(last);
  const noSpaceBefore = /\s/.test(first) || '.,;:!?)]}%"\'”’/-'.includes(first);
  return !noSpaceAfter && !noSpaceBefore;
}

function clampNumber(value, min, max) {
  if (!Number.isFinite(value)) return min;
  return Math.max(min, Math.min(max, value));
}

function roundPdfNumber(value) {
  return Math.round((Number(value) || 0) * 100) / 100;
}

function buildPdfSelectionRects(container) {
  const selection = window.getSelection?.();
  if (!container || !selection || selection.isCollapsed || selection.rangeCount === 0) return [];
  const range = selection.getRangeAt(0);
  if (!nodeInside(container, range.commonAncestorContainer)) return [];
  const rects = [];
  const spans = Array.from(container.querySelectorAll('.pdfium-text-layer span'));
  for (const span of spans) {
    if (!rangeIntersectsElement(range, span)) continue;
    const text = span.textContent || '';
    if (!text) continue;
    let start = 0;
    let end = text.length;
    if (nodeInside(span, range.startContainer)) {
      start = textOffsetWithin(span, range.startContainer, range.startOffset);
    }
    if (nodeInside(span, range.endContainer)) {
      end = textOffsetWithin(span, range.endContainer, range.endOffset);
    }
    if (end <= start) continue;
    const fontSize = parseFloat(span.style.fontSize || '0') || parseFloat(window.getComputedStyle(span).fontSize || '0') || 8;
    const fontFamily = span.style.fontFamily || window.getComputedStyle(span).fontFamily || 'serif';
    const totalWidth = measurePdfOverlayTextWidth(text, fontSize, fontFamily) || estimatePdfTextWidth(text, fontSize) || 1;
    const prefixWidth = measurePdfOverlayTextWidth(text.slice(0, start), fontSize, fontFamily) || estimatePdfTextWidth(text.slice(0, start), fontSize);
    const selectedWidth = measurePdfOverlayTextWidth(text.slice(start, end), fontSize, fontFamily) || estimatePdfTextWidth(text.slice(start, end), fontSize);
    const left = parseFloat(span.style.left || '0') || 0;
    const top = parseFloat(span.style.top || '0') || 0;
    const width = parseFloat(span.style.width || '0') || span.getBoundingClientRect().width || 0;
    const height = parseFloat(span.style.height || '0') || span.getBoundingClientRect().height || 0;
    if (!(width > 0) || !(height > 0)) continue;
    const layer = span.closest('.pdfium-text-layer');
    rects.push({
      pageNumber: Number(layer?.dataset?.pdfPage || 0) || undefined,
      left: left + (prefixWidth / totalWidth) * width,
      top,
      width: Math.max(1, (selectedWidth / totalWidth) * width),
      height
    });
  }
  return mergePdfSelectionRects(rects);
}

function mergePdfSelectionRects(rects) {
  const merged = [];
  for (const rect of rects) {
    const previous = merged.at(-1);
    if (
      previous
      && previous.pageNumber === rect.pageNumber
      && Math.abs(previous.top - rect.top) < 1.5
      && Math.abs(previous.height - rect.height) < 1.5
      && rect.left - (previous.left + previous.width) < 3
    ) {
      previous.width = Math.max(previous.width, rect.left + rect.width - previous.left);
    } else {
      merged.push({ ...rect });
    }
  }
  return merged;
}

function rangeIntersectsElement(range, element) {
  try {
    return range.intersectsNode(element);
  } catch {
    return false;
  }
}

function nodeInside(parent, node) {
  return Boolean(parent && node && (node === parent || parent.contains(node)));
}

function textOffsetWithin(container, node, offset) {
  if (!nodeInside(container, node)) return 0;
  const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
  let total = 0;
  while (walker.nextNode()) {
    const current = walker.currentNode;
    if (current === node) {
      return total + Math.max(0, Math.min(offset, current.textContent?.length || 0));
    }
    total += current.textContent?.length || 0;
  }
  if (node === container) {
    return Array.from(container.childNodes).slice(0, offset).reduce((sum, child) => sum + (child.textContent?.length || 0), 0);
  }
  return total;
}

function normalizePdfTextRuns(runs, scaleFactor) {
  return runs
    .map((run) => {
      const rect = run?.rect || {};
      const origin = rect.origin || {};
      const size = rect.size || {};
      const text = String(run?.text || '').replace(/\s+/g, ' ').trim();
      if (!text) return null;
      const width = Number(size.width || 0) * scaleFactor;
      const height = Number(size.height || 0) * scaleFactor;
      if (!(width > 0) || !(height > 0)) return null;
      const fontSize = Math.max(3, Math.round(height * 100) / 100);
      const fontFamily = pdfOverlayFontFamily(run?.font?.familyName || run?.font?.name || '');
      const measuredTextWidth = measurePdfOverlayTextWidth(text, fontSize, fontFamily) || estimatePdfTextWidth(text, fontSize);
      const scaleX = measuredTextWidth > 0
        ? Math.max(0.2, Math.min(8, width / measuredTextWidth))
        : 1;
      return {
        text,
        left: Math.round(Number(origin.x || 0) * scaleFactor * 100) / 100,
        top: Math.round(Number(origin.y || 0) * scaleFactor * 100) / 100,
        width: Math.max(1, Math.round(width * 100) / 100),
        height: Math.max(1, Math.round(height * 100) / 100),
        fontSize,
        scaleX: Math.round(scaleX * 1000) / 1000,
        fontFamily
      };
    })
    .filter(Boolean)
    .slice(0, 8_000);
}

let pdfTextMeasureCanvas = null;

function measurePdfOverlayTextWidth(text, fontSize, fontFamily) {
  try {
    if (!pdfTextMeasureCanvas) {
      pdfTextMeasureCanvas = document.createElement('canvas');
    }
    const context = pdfTextMeasureCanvas.getContext('2d');
    if (!context) return 0;
    context.font = `${fontSize}px ${fontFamily || 'serif'}`;
    return context.measureText(String(text || '')).width;
  } catch {
    return 0;
  }
}

function estimatePdfTextWidth(text, fontSize) {
  let units = 0;
  for (const char of String(text || '')) {
    if (/[\u4e00-\u9fff\u3040-\u30ff\uac00-\ud7af]/.test(char)) {
      units += 1;
    } else if (/\s/.test(char)) {
      units += 0.32;
    } else if (/[ilI.,:;!|]/.test(char)) {
      units += 0.28;
    } else if (/[mwMW@#%&]/.test(char)) {
      units += 0.88;
    } else {
      units += 0.56;
    }
  }
  return units * fontSize;
}

function pdfOverlayFontFamily(name) {
  const value = String(name || '').replace(/^[A-Z0-9]{6}\+/, '').trim();
  if (!value) return 'serif';
  if (/Times/i.test(value)) return 'Times New Roman, serif';
  if (/Courier/i.test(value)) return 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace';
  if (/Helvetica|Arial/i.test(value)) return 'Arial, Helvetica, sans-serif';
  return 'serif';
}

function locatorFromEmbedPdfSelection(formatted) {
  const rects = [];
  for (const item of formatted || []) {
    const page = Number(item?.pageIndex ?? 0) + 1;
    const sourceRects = item?.segmentRects?.length ? item.segmentRects : item?.rect ? [item.rect] : [];
    for (const rect of sourceRects) {
      const normalized = normalizeEmbedPdfRect(rect, page);
      if (normalized) rects.push(normalized);
      if (rects.length >= 32) break;
    }
    if (rects.length >= 32) break;
  }
  return {
    page: rects[0]?.page,
    rects
  };
}

function normalizeEmbedPdfRect(rect, page) {
  const origin = rect?.origin || {};
  const size = rect?.size || {};
  const width = Number(size.width ?? rect?.width ?? 0);
  const height = Number(size.height ?? rect?.height ?? 0);
  if (!(width > 0) || !(height > 0)) return null;
  return {
    page,
    x: Math.round(Number(origin.x ?? rect?.x ?? 0)),
    y: Math.round(Number(origin.y ?? rect?.y ?? 0)),
    width: Math.round(width),
    height: Math.round(height)
  };
}

function taskToPromise(task, label = 'PDF task', timeoutMs = 20_000) {
  if (!task) return Promise.resolve(null);
  if (typeof task.toPromise === 'function') return withTimeout(task.toPromise(), timeoutMs, `${label} timed out`);
  if (typeof task.wait === 'function') {
    return withTimeout(new Promise((resolve, reject) => {
      task.wait(resolve, reject);
    }), timeoutMs, `${label} timed out`);
  }
  if (typeof task.then === 'function') return withTimeout(task, timeoutMs, `${label} timed out`);
  return Promise.resolve(task);
}

function withTimeout(promise, timeoutMs, message) {
  let timeoutId = null;
  const timeout = new Promise((_, reject) => {
    timeoutId = window.setTimeout(() => reject(new Error(message)), timeoutMs);
  });
  return Promise.race([promise, timeout]).finally(() => {
    if (timeoutId) window.clearTimeout(timeoutId);
  });
}

function requestIdleCallbackSafe(callback) {
  if (typeof window !== 'undefined' && typeof window.requestIdleCallback === 'function') {
    return window.requestIdleCallback(callback, { timeout: 250 });
  }
  return window.setTimeout(() => callback({ didTimeout: true, timeRemaining: () => 0 }), 0);
}

function cancelIdleCallbackSafe(id) {
  if (!id) return;
  if (typeof window !== 'undefined' && typeof window.cancelIdleCallback === 'function') {
    window.cancelIdleCallback(id);
    return;
  }
  window.clearTimeout(id);
}

function absoluteUrlForWorker(url) {
  try {
    return new URL(url, window.location.href).href;
  } catch {
    return url;
  }
}

function createPdfDebugLogger(addDebugEntry) {
  const emit = (level, source, category, args) => {
    addDebugEntry(level, `PDFium ${source}/${category}`, args);
  };
  return {
    isEnabled: () => true,
    debug: (source, category, ...args) => emit('info', source, category, args),
    info: (source, category, ...args) => emit('info', source, category, args),
    warn: (source, category, ...args) => emit('warn', source, category, args),
    error: (source, category, ...args) => emit('error', source, category, args),
    perf: (source, category, event, phase, ...args) => emit('info', source, category, [event, phase, ...args])
  };
}

function closePdfEngine(engine) {
  if (!engine) return;
  try {
    const closeTask = engine.closeAllDocuments?.();
    if (closeTask?.wait) {
      closeTask.wait(
        () => engine.destroy?.()?.wait?.(() => {}, () => {}),
        () => engine.destroy?.()?.wait?.(() => {}, () => {})
      );
      return;
    }
    engine.destroy?.()?.wait?.(() => {}, () => {});
  } catch {
    // Best effort cleanup during preview teardown.
  }
}

function createPdfRenderWorker(engine, doc, primary = false) {
  return {
    engine,
    doc,
    primary,
    inFlight: 0,
    pageByNumber: new Map((doc?.pages || []).map((page) => [page.index + 1, page]))
  };
}

function acquirePdfRenderWorker(runtime) {
  const workers = runtime?.renderWorkers || [];
  if (workers.length === 0) return null;
  let best = workers[0];
  for (const worker of workers) {
    if ((worker.inFlight || 0) < (best.inFlight || 0)) best = worker;
  }
  return best;
}

async function warmPdfRenderWorkers(runtime, { wasmUrl, pdfBuffer, documentId, targetCount, addDebugEntry, scheduleRender }) {
  if (!runtime || runtime.disposed || runtime.renderWorkersWarming) return;
  runtime.renderWorkersWarming = true;
  const missing = Math.max(0, targetCount - (runtime.renderWorkers?.length || 0));
  if (missing === 0) {
    runtime.renderWorkersWarming = false;
    return;
  }
  const jobs = [];
  for (let index = 0; index < missing; index += 1) {
    const workerIndex = (runtime.renderWorkers?.length || 0) + index + 1;
    jobs.push(createPdfRenderWorkerDocument({
      wasmUrl,
      pdfBuffer,
      documentId: `${documentId}-render-${workerIndex}`,
      workerIndex
    }));
  }
  const results = await Promise.allSettled(jobs);
  if (runtime.disposed) {
    for (const result of results) {
      if (result.status === 'fulfilled') closePdfEngine(result.value.engine);
    }
    return;
  }
  let added = 0;
  for (const result of results) {
    if (result.status === 'fulfilled') {
      runtime.renderWorkers.push(result.value);
      added += 1;
    } else {
      addDebugEntry?.('warn', 'PDF render worker warmup failed', result.reason);
    }
  }
  runtime.renderWorkersWarming = false;
  if (added > 0) {
    addDebugEntry?.('info', 'PDF render worker pool ready', {
      workers: runtime.renderWorkers.length,
      concurrency: Math.min(PDF_RENDER_CONCURRENCY, runtime.renderWorkers.length)
    });
    scheduleRender?.();
  }
}

async function createPdfRenderWorkerDocument({ wasmUrl, pdfBuffer, documentId, workerIndex }) {
  const { createPdfiumEngine } = await import('@embedpdf/engines/pdfium-worker-engine');
  const engine = await withTimeout(
    Promise.resolve(createPdfiumEngine(wasmUrl, { encoderPoolSize: 0, fontFallback: null })),
    20_000,
    `PDF render worker ${workerIndex} initialization timed out`
  );
  try {
    const doc = await taskToPromise(
      engine.openDocumentBuffer({
        id: documentId,
        content: pdfBuffer.slice(0)
      }, { normalizeRotation: true }),
      `openRenderWorkerDocument:${workerIndex}`,
      20_000
    );
    return createPdfRenderWorker(engine, doc, false);
  } catch (error) {
    closePdfEngine(engine);
    throw error;
  }
}

function closePdfImage(image) {
  try {
    image?.bitmap?.close?.();
  } catch {
    // Best effort cleanup during preview teardown.
  }
}

function closePdfRuntime(runtime) {
  if (!runtime) return;
  runtime.disposed = true;
  if (runtime.renderPumpTimer) {
    window.clearTimeout(runtime.renderPumpTimer);
    runtime.renderPumpTimer = 0;
  }
  if (runtime.pageMetaCommitFrame) {
    cancelAnimationFrame(runtime.pageMetaCommitFrame);
    runtime.pageMetaCommitFrame = 0;
  }
  runtime.pendingPageMeta?.clear?.();
  if (runtime.selectionIndexTimer) {
    cancelIdleCallbackSafe(runtime.selectionIndexTimer);
    runtime.selectionIndexTimer = 0;
  }
  for (const timerId of runtime.textGeometryTimers?.values?.() || []) {
    cancelIdleCallbackSafe(timerId);
  }
  runtime.textGeometryTimers?.clear?.();
  runtime.textGeometryQueued?.clear?.();
  revokeObjectUrls(runtime.renderedUrls || []);
  for (const image of runtime.pageImageByNumber?.values?.() || []) {
    closePdfImage(image);
  }
  runtime.pageImageByNumber?.clear();
  const closed = new Set();
  for (const worker of runtime.renderWorkers || []) {
    if (!worker?.engine || closed.has(worker.engine)) continue;
    closed.add(worker.engine);
    closePdfEngine(worker.engine);
  }
  if (runtime.engine && !closed.has(runtime.engine)) closePdfEngine(runtime.engine);
}

function PdfSelectionDebugOverlay({ debug, detail }) {
  if (!debug) return null;
  return (
    <div className="pdf-selection-debug-overlay" aria-hidden="true">
      {(debug.blocks || []).map((block) => (
        <span
          className={`pdf-selection-debug-block ${block.type || 'unknown'}${detail?.kind === 'block' && detail.key === block.key ? ' focused' : ''}`}
          key={`block-${block.key}`}
          style={{
            left: `${block.rect.left}px`,
            top: `${block.rect.top}px`,
            width: `${block.rect.width}px`,
            height: `${block.rect.height}px`
          }}
        />
      ))}
      {debug.lanes.map((lane) => (
        <span
          className={`pdf-selection-debug-lane${detail?.kind === 'lane' && detail.key === lane.key ? ' focused' : ''}`}
          key={`lane-${lane.key}`}
          style={{
            left: `${lane.rect.left}px`,
            top: `${lane.rect.top}px`,
            width: `${lane.rect.width}px`,
            height: `${lane.rect.height}px`
          }}
        />
      ))}
      {debug.lines.map((line) => (
        <span
          className={`pdf-selection-debug-line${detail?.kind === 'line' && detail.key === line.key ? ' focused' : ''}`}
          key={`line-${line.key}`}
          style={{
            left: `${line.rect.left}px`,
            top: `${line.rect.top}px`,
            width: `${line.rect.width}px`,
            height: `${line.rect.height}px`
          }}
        />
      ))}
      {debug.glyphs.map((glyph) => (
        <span
          className={`pdf-selection-debug-glyph${glyph.selected ? ' selected' : ''}${detail?.kind === 'glyph' && detail.key === glyph.key ? ' focused' : ''}`}
          key={`glyph-${glyph.key}`}
          style={{
            left: `${glyph.rect.left}px`,
            top: `${glyph.rect.top}px`,
            width: `${glyph.rect.width}px`,
            height: `${glyph.rect.height}px`
          }}
        />
      ))}
      {debug.glyphs.map((glyph) => (
        <span
          className="pdf-selection-debug-hit"
          key={`hit-${glyph.key}`}
          style={{
            left: `${glyph.hitRect.left}px`,
            top: `${glyph.hitRect.top}px`,
            width: `${glyph.hitRect.width}px`,
            height: `${glyph.hitRect.height}px`
          }}
        />
      ))}
      {debug.selectionRects.map((entry) => (
        <span
          className={`pdf-selection-debug-selection${detail?.kind === 'selection' && detail.key === entry.key ? ' focused' : ''}`}
          key={`selection-${entry.key}`}
          style={{
            left: `${entry.rect.left}px`,
            top: `${entry.rect.top}px`,
            width: `${entry.rect.width}px`,
            height: `${entry.rect.height}px`
          }}
        />
      ))}
      {debug.points.map((point) => (
        <span
          className={`pdf-selection-debug-point ${point.kind}`}
          key={point.key}
          style={{
            left: `${point.x}px`,
            top: `${point.y}px`
          }}
        >
          {point.kind === 'anchor' ? 'A' : 'F'}
        </span>
      ))}
      {detail?.rect ? (
        <span
          className="pdf-selection-debug-popover"
          style={debugPopoverStyle(detail.rect)}
        >
          {debugDetailTitle(detail)}
        </span>
      ) : null}
    </div>
  );
}

function debugPopoverStyle(rect) {
  return {
    left: `${Math.max(0, rect.left)}px`,
    top: `${Math.max(0, rect.top - 24)}px`
  };
}

function debugDetailTitle(detail) {
  const data = detail.detail || {};
  if (detail.kind === 'glyph') {
    return `glyph "${data.char}" order:${data.order} line:${data.lineId || '-'} block:${data.blockType || '-'} lane:${data.laneIndex}`;
  }
  if (detail.kind === 'line') {
    return `line:${data.lineId || '-'} block:${data.blockType || '-'} lane:${data.laneIndex} row:${data.row ?? '-'} n:${data.glyphCount}`;
  }
  if (detail.kind === 'block') {
    return `block:${data.blockId || '-'} type:${data.blockType || '-'} lane:${data.laneIndex} lines:${data.lineCount}`;
  }
  if (detail.kind === 'lane') {
    return `lane:${data.laneIndex} left:${data.left} right:${data.right} margin:${data.margin}`;
  }
  return 'selection rect';
}

function PdfiumPageCanvas({ image, imageKey, label, pageNumber, onPaint }) {
  const canvasRef = useRef(null);

  useLayoutEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !(image?.width > 0) || !(image?.height > 0)) return undefined;
    canvas.width = image.width;
    canvas.height = image.height;
    const context = canvas.getContext('2d', { alpha: false });
    if (!context) return undefined;
    const paintStartAt = performance.now();
    if (image.bitmap) {
      context.drawImage(image.bitmap, 0, 0);
    } else if (image.data) {
      const imageData = new ImageData(
        image.data instanceof Uint8ClampedArray ? image.data : new Uint8ClampedArray(image.data),
        image.width,
        image.height
      );
      context.putImageData(imageData, 0, 0);
    }
    onPaint?.(pageNumber, imageKey, performance.now() - paintStartAt);
    return undefined;
  }, [image, imageKey, onPaint, pageNumber]);

  return <canvas ref={canvasRef} aria-label={label} className="pdfium-page-canvas" />;
}

function revokeObjectUrls(urls = []) {
  urls.forEach((url) => {
    if (typeof url === 'string' && url.startsWith('blob:')) {
      URL.revokeObjectURL(url);
    }
  });
}

function stringifyDebugDetail(value) {
  if (value instanceof Error) {
    return `${value.name}: ${value.message}${value.stack ? `\n${value.stack}` : ''}`;
  }
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value, (_key, nested) => {
      if (nested instanceof Error) {
        return {
          name: nested.name,
          message: nested.message,
          stack: nested.stack
        };
      }
      if (typeof nested === 'function') return `[Function ${nested.name || 'anonymous'}]`;
      return nested;
    }, 2);
  } catch {
    return String(value);
  }
}

function handleCodeSelectionContextMenu(event, container, file, sourceKind, sourceText, onSelectionContextMenu) {
  const reference = buildCodeSelectionReference(container, file, sourceKind, sourceText);
  if (reference) onSelectionContextMenu?.(event, reference);
}

function handleDomSelectionContextMenu(event, container, file, sourceKind, sourceText, onSelectionContextMenu) {
  const reference = buildDomSelectionReference(container, file, sourceKind, sourceText);
  if (reference) onSelectionContextMenu?.(event, reference);
}

function handlePreviewSurfaceContextMenu(event, file, onSelectionContextMenu) {
  if (event.defaultPrevented || !file) return;
  const target = event.target;
  if (!(target instanceof Element)) return;

  const codeNode = target.closest('.file-code-view');
  if (codeNode) {
    const sourceKind = codeNode.getAttribute('data-selection-kind') || (file.kind === 'html' ? 'html' : 'code');
    handleCodeSelectionContextMenu(event, codeNode, file, sourceKind, file.content || file.data || '', onSelectionContextMenu);
    return;
  }

  const domNode = target.closest('.markdown-preview');
  if (domNode) {
    const sourceKind = domNode.getAttribute('data-selection-kind') || 'markdown';
    handleDomSelectionContextMenu(event, domNode, file, sourceKind, domNode.textContent || file.content || file.data || '', onSelectionContextMenu);
  }
}

function preserveSelectionOnRightPointerDown(event) {
  if (event.button !== 2) return;
  const selection = window.getSelection?.();
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return;
  const range = selection.getRangeAt(0);
  if (event.currentTarget.contains(range.commonAncestorContainer)) {
    event.preventDefault();
    event.stopPropagation();
  }
}

function selectionWithin(container) {
  if (!container) return null;
  const selection = window.getSelection?.();
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  const range = selection.getRangeAt(0);
  if (!container.contains(range.commonAncestorContainer)) return null;
  const text = String(selection.toString() || '').trim();
  if (!text) return null;
  const prefixRange = range.cloneRange();
  prefixRange.selectNodeContents(container);
  prefixRange.setEnd(range.startContainer, range.startOffset);
  const startOffset = prefixRange.toString().length;
  return { text, range, startOffset };
}

function buildSelectionReference({ file, sourceKind, selectedText, sourceText, startOffset = 0, locator = {} }) {
  const originalTextLength = String(selectedText || '').length;
  const cappedText = clampText(selectedText, 4_000);
  const context = contextAround(sourceText, startOffset, originalTextLength);
  return {
    id: `${file?.path || file?.name || 'selection'}-${Date.now()}-${Math.random().toString(36).slice(2)}`,
    file_path: file?.path || file?.name || '',
    file_name: file?.name || fileNameFromPath(file?.path || ''),
    media_type: file?.media_type || '',
    source_kind: sourceKind || file?.kind || 'text',
    selected_text: cappedText,
    original_text_length: originalTextLength,
    locator,
    context
  };
}

function contextAround(sourceText, startOffset, length) {
  const source = String(sourceText || '');
  if (!source) return undefined;
  const before = source.slice(Math.max(0, startOffset - 700), Math.max(0, startOffset));
  const after = source.slice(Math.max(0, startOffset + length), startOffset + length + 700);
  return {
    before: clampText(before, 700),
    after: clampText(after, 700)
  };
}

function lineLocatorForOffset(sourceText, startOffset, length) {
  const source = String(sourceText || '');
  const before = source.slice(0, Math.max(0, startOffset));
  const selectedPrefix = source.slice(0, Math.max(0, startOffset + length));
  const startLine = before.split(/\n/).length;
  const endLine = selectedPrefix.split(/\n/).length;
  const lastNewline = before.lastIndexOf('\n');
  const startColumn = startOffset - lastNewline;
  const endLinePrefix = source.slice(0, Math.max(0, startOffset + length));
  const endLastNewline = endLinePrefix.lastIndexOf('\n');
  const endColumn = startOffset + length - endLastNewline;
  return {
    start_line: startLine,
    end_line: endLine,
    start_column: startColumn,
    end_column: endColumn,
    text_offset: startOffset,
    text_length: length
  };
}

function nearestSelectionHeading(container, range) {
  const headings = Array.from(container.querySelectorAll('h1,h2,h3,h4,h5,h6'));
  if (!headings.length) return undefined;
  const rangeRect = range.getBoundingClientRect();
  let candidate = null;
  for (const heading of headings) {
    const rect = heading.getBoundingClientRect();
    if (rect.top <= rangeRect.top + 1) candidate = heading;
  }
  return candidate?.textContent?.trim() || undefined;
}

function clampText(value, maxChars) {
  return String(value || '').trim().slice(0, maxChars);
}

function stableHash(value) {
  const text = String(value || '');
  let hash = 2166136261;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(36);
}

function arrayBufferFromPdfFile(file) {
  const value = file?.pdf_buffer;
  if (value instanceof ArrayBuffer) return value.slice(0);
  if (ArrayBuffer.isView(value)) {
    return value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength);
  }
  if (Array.isArray(value)) return new Uint8Array(value).buffer;
  const base64 = file?.encoding === 'base64' ? String(file.data || file.content || '') : '';
  return base64 ? base64ToArrayBuffer(base64) : null;
}

function selectionMenuStyle(menu) {
  const width = 170;
  const height = 76;
  const margin = 8;
  const left = Math.min(Math.max(margin, menu.x), Math.max(margin, window.innerWidth - width - margin));
  const top = Math.min(Math.max(margin, menu.y), Math.max(margin, window.innerHeight - height - margin));
  return { left, top };
}

function base64ToArrayBuffer(base64) {
  const binary = window.atob(String(base64 || '').replace(/\s/g, ''));
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes.buffer;
}

function htmlToPlainText(html) {
  const node = document.createElement('div');
  node.innerHTML = html;
  return node.textContent || '';
}

function highlightCode(code, language) {
  const value = String(code || '');
  const lang = String(language || '').replace(/^jsx$/, 'javascript').replace(/^tsx$/, 'typescript');
  try {
    if (lang && hljs.getLanguage(lang)) {
      return hljs.highlight(value, { language: lang, ignoreIllegals: true }).value;
    }
    return hljs.highlightAuto(value).value;
  } catch {
    return value.replace(/[&<>"']/g, (char) => ({
      '&': '&amp;',
      '<': '&lt;',
      '>': '&gt;',
      '"': '&quot;',
      "'": '&#039;'
    })[char]);
  }
}
