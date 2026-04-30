import { Fragment, useState } from 'react';
import { ChevronDown, ChevronRight, Download, FileText, Folder, RefreshCw, Trash2 } from 'lucide-react';
import * as ContextMenu from '@radix-ui/react-context-menu';
import { formatBytes } from '../lib/format';
import { normalizeWorkspacePath, workspaceAbsolutePath, workspaceDisplayRoot, workspaceEntryIcon, workspaceEntryKind } from '../lib/workspaceUtils';

export function WorkspacePanel({ open, selected, listings, expanded, loading, error, status, activeFilePath, onRefresh, onToggleDirectory, onOpenFile, onUpload, onDownload }) {
  const [dragOver, setDragOver] = useState(false);
  const rootListing = listings.get('');
  const rootEntries = rootListing?.entries || [];
  const rootLabel = workspaceDisplayRoot(rootListing, status);
  const modeLabel = rootListing?.remote
    ? `${rootListing.remote.host}${rootListing.remote.cwd ? ` · ${rootListing.remote.cwd}` : ''}`
    : rootListing?.mode || (selected ? 'local workspace' : '未选择 Conversation');
  const renderContextMenu = (entry) => (
    <ContextMenu.Portal>
      <ContextMenu.Content className="context-menu">
        {workspaceEntryKind(entry) !== 'directory' && (
          <ContextMenu.Item className="context-menu-item" onSelect={() => onOpenFile(entry)}>打开</ContextMenu.Item>
        )}
        <ContextMenu.Item className="context-menu-item" onSelect={() => navigator.clipboard?.writeText(entry.path || '')}>
          <FileText size={13} />
          复制相对路径
        </ContextMenu.Item>
        <ContextMenu.Item className="context-menu-item" onSelect={() => navigator.clipboard?.writeText(workspaceAbsolutePath(rootListing, status, entry.path))}>
          <Folder size={13} />
          复制绝对路径
        </ContextMenu.Item>
        <ContextMenu.Item className="context-menu-item" onSelect={() => onDownload(entry)}>
          <Download size={13} />
          下载
        </ContextMenu.Item>
        <ContextMenu.Item className="context-menu-item danger disabled" disabled>
          <Trash2 size={13} />
          删除
        </ContextMenu.Item>
      </ContextMenu.Content>
    </ContextMenu.Portal>
  );
  const renderEntry = (entry, level = 0) => {
    const kind = workspaceEntryKind(entry);
    const isDirectory = kind === 'directory';
    const path = normalizeWorkspacePath(entry.path);
    const isExpanded = expanded.has(path);
    const listing = listings.get(path);
    const children = listing?.entries || [];
    return (
      <Fragment key={path || entry.name}>
        <ContextMenu.Root>
          <ContextMenu.Trigger asChild>
            <button
              className={`file-tree-row${isDirectory ? ' directory' : ''}${activeFilePath === path ? ' active' : ''}`}
              style={{ '--tree-level': level }}
              type="button"
              onClick={() => (isDirectory ? onToggleDirectory(path) : onOpenFile(entry))}
              onDragOver={isDirectory ? (event) => {
                event.preventDefault();
                event.stopPropagation();
                event.dataTransfer.dropEffect = 'copy';
              } : undefined}
              onDrop={isDirectory ? (event) => {
                event.preventDefault();
                event.stopPropagation();
                const items = Array.from(event.dataTransfer?.items || []);
                if (items.length) onUpload(path, items);
              } : undefined}
            >
              <span className="tree-chevron">{isDirectory ? (isExpanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />) : null}</span>
              {workspaceEntryIcon(entry)}
              <span className="file-tree-name" title={entry.name}>{entry.name}</span>
              {!isDirectory && <small className="file-tree-size">{formatBytes(entry.size_bytes)}</small>}
            </button>
          </ContextMenu.Trigger>
          {renderContextMenu(entry)}
        </ContextMenu.Root>
        {isDirectory && isExpanded && loading.has(path) && (
          <div className="file-tree-empty" style={{ '--tree-level': level + 1 }}>正在读取...</div>
        )}
        {isDirectory && isExpanded && !loading.has(path) && listing && children.length === 0 && (
          <div className="file-tree-empty" style={{ '--tree-level': level + 1 }}>空文件夹</div>
        )}
        {isDirectory && isExpanded && children.map((child) => renderEntry(child, level + 1))}
      </Fragment>
    );
  };
  const handleDrop = (event) => {
    event.preventDefault();
    setDragOver(false);
    const items = Array.from(event.dataTransfer?.items || []);
    if (items.length) onUpload('', items);
  };
  return (
    <aside
      className={`right-panel workspace-panel${open ? ' open' : ''}${dragOver ? ' drag-over' : ''}`}
      aria-hidden={!open}
      onDragEnter={(event) => {
        event.preventDefault();
        setDragOver(true);
      }}
      onDragOver={(event) => {
        event.preventDefault();
        event.dataTransfer.dropEffect = 'copy';
      }}
      onDragLeave={(event) => {
        if (event.currentTarget === event.target) setDragOver(false);
      }}
      onDrop={handleDrop}
    >
      <header className="file-browser-header">
        <div className="file-browser-title">
          <strong>工作区文件</strong>
          <span>{modeLabel}</span>
        </div>
        <button className="panel-icon-button" type="button" onClick={onRefresh} title="刷新文件">
          <RefreshCw size={15} />
        </button>
      </header>
      <div className="file-browser-body tree-only">
        <section className="file-tree">
          <div className="file-tree-root">
            <Folder size={15} />
            <span>{rootLabel || 'workspace'}</span>
          </div>
          {error && <div className="file-tree-empty error">{error}</div>}
          {!rootListing && !error && <div className="file-tree-empty">正在读取工作区...</div>}
          {rootEntries.map((entry) => renderEntry(entry))}
        </section>
      </div>
    </aside>
  );
}
