import { Component } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';
import hljs from 'highlight.js';
import { X } from 'lucide-react';
import stellacodeMark from '../assets/stellacode-mark.svg';
import { fileExtension, fileNameFromPath, imageMimeType, isImageFile, isMarkdownFile } from '../lib/fileUtils';

export function FilePreviewPanel({ open, openFiles, activeFilePath, onSelectFile, onCloseFile }) {
  const activeFile = openFiles.find((file) => file.path === activeFilePath) || null;
  return (
    <aside className={`right-panel preview-panel${open ? ' open' : ''}`} aria-hidden={!open}>
      <header className="file-browser-header">
        <div>
          <strong>文件预览</strong>
          <span>{activeFile ? activeFile.path : '未打开文件'}</span>
        </div>
      </header>
      <section className="file-preview detached">
        <div className="editor-tabs">
          {openFiles.map((file) => (
            <button
              key={file.path}
              className={`editor-tab${activeFile?.path === file.path ? ' active' : ''}`}
              type="button"
              onClick={() => onSelectFile(file.path)}
            >
              <span>{file.name}</span>
              <span
                className="editor-tab-close"
                role="button"
                tabIndex={0}
                onClick={(event) => {
                  event.stopPropagation();
                  onCloseFile(file.path);
                }}
              >
                <X size={12} />
              </span>
            </button>
          ))}
        </div>
        <div className="preview-surface">
          {activeFile?.loading ? (
            <div className="panel-placeholder">正在读取文件...</div>
          ) : activeFile?.error ? (
            <div className="panel-placeholder">{activeFile.error}</div>
          ) : activeFile ? (
            <PreviewErrorBoundary resetKey={activeFile.path}>
              <FilePreview file={activeFile} />
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

function FilePreview({ file }) {
  const name = file.name || fileNameFromPath(file.path);
  const ext = fileExtension(name);
  const source = file.content || file.data || '';
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
  return (
    <CodePreview code={source} language={file.language || ext} />
  );
}

function MarkdownBlock({ text }) {
  return (
    <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
      {String(text || '')}
    </ReactMarkdown>
  );
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
