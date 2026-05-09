import { Component, useEffect, useMemo, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';
import hljs from 'highlight.js';
import { Code2, Download, Eye, FileText, Minus, Plus, Printer, X } from 'lucide-react';
import stellacodeMark from '../assets/stellacode-mark.svg';
import { fileExtension, fileNameFromPath, imageMimeType, isHtmlFile, isImageFile, isMarkdownFile, isPdfFile } from '../lib/fileUtils';

export function FilePreviewPanel({ open, openFiles, activeFilePath, onSelectFile, onCloseFile, onDownloadFile }) {
  const activeFile = openFiles.find((file) => file.path === activeFilePath) || null;
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
        <div className="preview-surface">
          {activeFile?.loading ? (
            <div className="panel-placeholder">正在读取文件...</div>
          ) : activeFile?.error ? (
            <div className="panel-placeholder">{activeFile.error}</div>
          ) : activeFile ? (
            <PreviewErrorBoundary resetKey={activeFile.path}>
              <FilePreview file={activeFile} onDownloadFile={onDownloadFile} />
            </PreviewErrorBoundary>
          ) : (
            <div className="panel-placeholder">打开一个文件查看预览</div>
          )}
        </div>
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

function FilePreview({ file, onDownloadFile }) {
  const name = file.name || fileNameFromPath(file.path);
  const ext = fileExtension(name);
  const source = file.content || file.data || '';
  if (file.kind === 'pdf' || isPdfFile(name)) {
    if (file.pdf_url) {
      return <PdfNativePreview file={file} name={name} onDownloadFile={onDownloadFile} />;
    }
    return (
      <div className="file-binary-preview">
        <FileText size={34} />
        <strong>{name}</strong>
        <span>无法在面板内加载这个 PDF，可以下载后查看。</span>
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
      <article className="markdown-preview">
        <MarkdownBlock text={source} />
      </article>
    );
  }
  if (file.kind === 'html' || isHtmlFile(name)) {
    return <HtmlPreview name={name} source={source} language={file.language || ext} />;
  }
  return (
    <CodePreview code={source} language={file.language || ext} />
  );
}

function HtmlPreview({ name, source, language }) {
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
          <CodePreview code={source} language={language || 'html'} />
        )}
      </div>
    </div>
  );
}

function PdfNativePreview({ file, name, onDownloadFile }) {
  const [zoomPercent, setZoomPercent] = useState(100);
  const [frameLoaded, setFrameLoaded] = useState(false);
  const viewerUrl = useMemo(
    () => pdfEmbedUrl(file.pdf_url, zoomPercent),
    [file.pdf_url, zoomPercent]
  );

  useEffect(() => {
    setFrameLoaded(false);
  }, [viewerUrl]);

  const zoomIn = () => {
    setZoomPercent((value) => Math.min(300, value + 15));
  };

  const zoomOut = () => {
    setZoomPercent((value) => Math.max(50, value - 15));
  };

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
            title="缩小"
            aria-label="缩小"
            onClick={zoomOut}
          >
            <Minus size={14} />
          </button>
          <button
            className="secondary-button icon-only"
            type="button"
            title="放大"
            aria-label="放大"
            onClick={zoomIn}
          >
            <Plus size={14} />
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
      <div className="native-pdf-shell">
        {!frameLoaded ? <div className="pdf-loading-mask">正在加载 PDF...</div> : null}
        <iframe
          className={`native-pdf-frame${frameLoaded ? ' loaded' : ''}`}
          title={name}
          src={viewerUrl}
          onLoad={() => {
            window.setTimeout(() => setFrameLoaded(true), 120);
          }}
        />
      </div>
    </div>
  );
}

function pdfEmbedUrl(url, zoomPercent = 100) {
  return `${url}#toolbar=0&navpanes=0&scrollbar=1&view=FitH&zoom=${Math.round(zoomPercent)}`;
}

function MarkdownBlock({ text }) {
  const parsed = useMemo(() => splitMarkdownMetadata(text), [text]);
  return (
    <>
      {parsed.metadata.length ? <MarkdownMetadata entries={parsed.metadata} /> : null}
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
        {parsed.body}
      </ReactMarkdown>
    </>
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

function CodePreview({ code, language }) {
  const highlighted = highlightCode(code, language);
  return (
    <pre className="file-code-view">
      <code className="hljs" dangerouslySetInnerHTML={{ __html: highlighted }} />
    </pre>
  );
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
