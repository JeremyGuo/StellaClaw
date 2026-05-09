import { Component, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import rehypeRaw from 'rehype-raw';
import remarkGfm from 'remark-gfm';
import hljs from 'highlight.js';
import mammoth from 'mammoth/mammoth.browser';
import { renderAsync as renderDocxAsync } from 'docx-preview';
import embedPdfiumWasmUrl from '@embedpdf/pdfium/pdfium.wasm?url';
import { Code2, Download, Eye, FileText, Info, Printer, RefreshCw, X } from 'lucide-react';
import stellacodeMark from '../assets/stellacode-mark.svg';
import { handleExternalLinkClick, isExternalUrl } from '../lib/externalLinks';
import { fileExtension, fileNameFromPath, imageMimeType, isHtmlFile, isImageFile, isMarkdownFile, isPdfFile, isPresentationFile, isWordFile } from '../lib/fileUtils';

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
  const pdfSelectionFrameRef = useRef(0);
  const [wasmUrl, setWasmUrl] = useState('');
  const [wasmError, setWasmError] = useState('');
  const [debugEntries, setDebugEntries] = useState([]);
  const [shellWidth, setShellWidth] = useState(0);
  const zoomPercent = 100;
  const [showInfoPanel, setShowInfoPanel] = useState(false);
  const [pdfSelectionRects, setPdfSelectionRects] = useState([]);
  const [renderState, setRenderState] = useState({
    loading: true,
    pages: [],
    pageCount: 0,
    error: ''
  });
  const documentId = useMemo(() => `preview-${stableHash(file.path || name)}`, [file.path, name]);
  const pdfBuffer = useMemo(() => arrayBufferFromPdfFile(file), [file]);
  const addDebugEntry = (level, message, detail) => {
    const entry = {
      at: new Date().toLocaleTimeString(),
      level,
      message,
      detail: detail ? stringifyDebugDetail(detail) : ''
    };
    setDebugEntries((entries) => [...entries.slice(-15), entry]);
    const logger = level === 'error' ? console.error : level === 'warn' ? console.warn : console.info;
    logger('[StellaCodeX PDF]', message, detail || '');
  };

  useEffect(() => {
    let disposed = false;
    setWasmUrl('');
    setWasmError('');
    setDebugEntries([]);
    addDebugEntry('info', 'Using Vite PDFium WASM asset URL', { url: embedPdfiumWasmUrl });
    if (!disposed) setWasmUrl(embedPdfiumWasmUrl);
    return () => {
      disposed = true;
    };
  }, [file.pdf_url]);

  useEffect(() => {
    const node = shellRef.current;
    if (!node) return undefined;
    let timeoutId = 0;
    let lastWidth = 0;
    const applyWidth = (width) => {
      if (!width || Math.abs(width - lastWidth) < 8) return;
      lastWidth = width;
      setShellWidth(width);
    };
    const update = () => {
      if (document.querySelector('.app-root.layout-resizing')) {
        window.clearTimeout(timeoutId);
        timeoutId = window.setTimeout(update, 220);
        return;
      }
      const width = Math.max(0, Math.floor(node.clientWidth || 0));
      if (!lastWidth) {
        applyWidth(width);
        return;
      }
      window.clearTimeout(timeoutId);
      timeoutId = window.setTimeout(() => applyWidth(width), 260);
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return () => {
      window.clearTimeout(timeoutId);
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
    const localUrls = [];
    const availableWidth = shellWidth > 0 ? shellWidth : 820;
    const basePadding = Math.min(10, Math.max(4, availableWidth * 0.01));
    const render = async () => {
      const previousUrls = renderUrlsRef.current;
      setRenderState((current) => ({ ...current, loading: true, error: '' }));
      try {
        addDebugEntry('info', 'Creating PDFium direct engine', { wasmUrl, pdfBytes: pdfBuffer.byteLength, shellWidth: availableWidth });
        const { createPdfiumEngine } = await import('@embedpdf/engines/pdfium-direct-engine');
        engine = await withTimeout(createPdfiumEngine(wasmUrl), 20_000, 'PDFium engine initialization timed out');
        if (disposed) return;
        addDebugEntry('info', 'PDFium direct engine created');
        addDebugEntry('info', 'Opening PDF document buffer', { documentId, pdfBytes: pdfBuffer.byteLength });
        const doc = await taskToPromise(
          engine.openDocumentBuffer({
            id: documentId,
            content: pdfBuffer.slice(0)
          }, { normalizeRotation: true }),
          'openDocumentBuffer',
          20_000
        );
        if (disposed) return;
        addDebugEntry('info', 'PDF document opened', { pageCount: doc.pageCount });
        setRenderState((current) => ({ ...current, loading: true, pageCount: doc.pageCount, error: '' }));
        const nextPages = [];
        for (const page of doc.pages) {
          if (disposed) return;
          const fitScale = Math.max(0.2, Math.min(3, (availableWidth - basePadding * 2) / Math.max(1, page.size.width)));
          const scaleFactor = fitScale * (zoomPercent / 100);
          const dpr = Math.min(window.devicePixelRatio || 1, 2);
          addDebugEntry('info', 'Rendering PDF page', { page: page.index + 1, scaleFactor: Number(scaleFactor.toFixed(3)), dpr });
          const blob = await taskToPromise(
            engine.renderPage(doc, page, {
              scaleFactor,
              dpr,
              imageType: 'image/png',
              withAnnotations: true,
              withForms: true
            }),
            `renderPage:${page.index + 1}`,
            30_000
          );
          if (disposed) return;
          const url = URL.createObjectURL(blob);
          localUrls.push(url);
          let textRuns = [];
          try {
            const pageText = await taskToPromise(
              engine.getPageTextRuns(doc, page),
              `getPageTextRuns:${page.index + 1}`,
              15_000
            );
            textRuns = normalizePdfTextRuns(pageText?.runs || [], scaleFactor);
          } catch (textError) {
            addDebugEntry('warn', 'PDF text layer failed', { page: page.index + 1, error: textError });
          }
          const nextPage = {
            pageNumber: page.index + 1,
            width: Math.max(1, Math.round(page.size.width * scaleFactor)),
            height: Math.max(1, Math.round(page.size.height * scaleFactor)),
            url,
            textRuns
          };
          nextPages.push(nextPage);
          setRenderState((current) => ({
            ...current,
            loading: true,
            pageCount: doc.pageCount
          }));
        }
        renderUrlsRef.current = localUrls;
        addDebugEntry('info', 'PDF pages rendered', { pageCount: doc.pageCount });
        setRenderState({
          loading: false,
          pages: nextPages,
          pageCount: doc.pageCount,
          error: ''
        });
        revokeObjectUrls(previousUrls);
      } catch (error) {
        if (!disposed) {
          addDebugEntry('error', 'PDFium render failed', error);
          revokeObjectUrls(localUrls);
          setRenderState((current) => ({
            ...current,
            loading: false,
            error: error?.message || 'PDFium 渲染失败'
          }));
        }
      }
    };
    render();
    return () => {
      disposed = true;
      revokeObjectUrls(localUrls);
      closePdfEngine(engine);
    };
  }, [documentId, pdfBuffer, shellWidth, wasmUrl, zoomPercent]);

  useEffect(() => () => {
    if (pdfSelectionFrameRef.current) {
      cancelAnimationFrame(pdfSelectionFrameRef.current);
      pdfSelectionFrameRef.current = 0;
    }
    revokeObjectUrls(renderUrlsRef.current);
    renderUrlsRef.current = [];
  }, []);

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
    const point = pdfPointFromEvent(event);
    if (!point) return;
    event.preventDefault();
    window.getSelection?.()?.removeAllRanges();
    pdfDragRef.current = {
      pointerId: event.pointerId,
      start: point,
      current: point,
      moved: false,
      runs: collectPdfTextRuns(event.currentTarget).filter(isSelectablePdfTextRun)
    };
    pdfSelectionRef.current = null;
    setPdfSelectionRects([]);
    event.currentTarget.setPointerCapture?.(event.pointerId);
  };

  const applyPdfDragSelection = (drag, current) => {
    const shell = shellRef.current;
    if (!drag || !shell) return;
    const selection = buildPdfDragSelection(shell, drag.start, current, drag.runs);
    pdfSelectionRef.current = selection;
    setPdfSelectionRects(selection?.rects || []);
  };

  const handlePdfPointerMove = (event) => {
    const drag = pdfDragRef.current;
    const shell = shellRef.current;
    if (!drag || drag.pointerId !== event.pointerId || !shell) return;
    const current = pdfPointFromEvent(event, shell);
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
    const current = pdfPointFromEvent(event, shell);
    pdfDragRef.current = null;
    event.currentTarget.releasePointerCapture?.(event.pointerId);
    if (!current || !drag.moved) {
      pdfSelectionRef.current = null;
      setPdfSelectionRects([]);
      return;
    }
    event.preventDefault();
    if (pdfSelectionFrameRef.current) {
      cancelAnimationFrame(pdfSelectionFrameRef.current);
      pdfSelectionFrameRef.current = 0;
    }
    applyPdfDragSelection(drag, current);
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
    <div className="pdf-preview">
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
                className="pdfium-page"
                key={`${documentId}-${page.pageNumber}-${page.url}`}
                style={{ width: `${page.width}px`, height: `${page.height}px` }}
              >
                <img src={page.url} alt={`${name} page ${page.pageNumber}`} draggable={false} />
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
                  {pdfSelectionRects.filter((rect) => rect.pageNumber === page.pageNumber).map((rect, index) => (
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
                <span className="pdfium-page-number">{page.pageNumber}</span>
              </div>
            ))}
          </div>
        )}
        {renderState.loading && !showInfoPanel ? (
          <div className="pdf-loading-status">
            PDFium 渲染中{renderState.pageCount ? ` ${renderState.pages.length}/${renderState.pageCount}` : ''}
          </div>
        ) : null}
        {showInfoPanel ? (
          <div className="pdf-debug-overlay">
            <strong>{renderState.loading ? `PDFium 渲染中${renderState.pageCount ? `：${renderState.pages.length}/${renderState.pageCount}` : ''}` : 'PDF 信息'}</strong>
            <span>当前使用 direct engine，绕过 EmbedPDF snippet 插件初始化。</span>
            <pre>{debugEntries.map((entry) => (
              `[${entry.at}] ${entry.level.toUpperCase()} ${entry.message}${entry.detail ? `\n${entry.detail}` : ''}`
            )).join('\n\n')}</pre>
          </div>
        ) : null}
      </div>
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
