import { useEffect, useMemo, useState } from 'react';
import { Copy, Download, FileSearch, Folder, MessageSquarePlus, Monitor, PanelLeft, ScrollText, TerminalSquare, Trash2, Upload } from 'lucide-react';
import * as Popover from '@radix-ui/react-popover';
import { clearChatProtocolDiagnostics, readChatProtocolDiagnostics } from '../lib/chatProtocolDiagnostics';
import { chatRawRenderSnapshot, chatRenderOverviewText } from '../lib/chatRenderDiagnostics';

export function WindowChrome({
  title,
  subtitle,
  transfers,
  sidebarMode,
  onToggleSidebar,
  onNewConversation,
  overviewPanelOpen,
  workspacePanelOpen,
  previewPanelOpen,
  rawRenderMessages = [],
  updateReady = false,
  onToggleOverview,
  onToggleWorkspace,
  onTogglePreview,
  onToggleTerminal,
  onInstallUpdate
}) {
  return (
    <header className={`window-chrome${updateReady ? ' update-ready' : ''}`}>
      <div className="platform-safe-area" />
      <div className="left-toolbar">
        <button className="chrome-button" type="button" onClick={onToggleSidebar} title={sidebarMode === 'collapsed' ? '显示 Conversation Bar' : '隐藏 Conversation Bar'}>
          <PanelLeft size={18} />
        </button>
        <button className="chrome-button new-chat-button" type="button" onClick={onNewConversation} title="新对话">
          <MessageSquarePlus size={18} />
          <span>新对话</span>
        </button>
      </div>
      <div className="title-track">
        <div className="drag-strip" />
        <div className="title-text">
          <strong>{title}</strong>
          <span>{subtitle}</span>
        </div>
      </div>
      <div className="right-toolbar">
        {updateReady && (
          <button className="chrome-update-button" type="button" onClick={onInstallUpdate} title="安装更新并重启">
            Update
          </button>
        )}
        <ProtocolLogButton rawRenderMessages={rawRenderMessages} />
        <TransferButton transfers={transfers} />
        <button className="chrome-button" type="button" onClick={onToggleTerminal} title="终端">
          <TerminalSquare size={18} />
        </button>
        <button className={`chrome-button${overviewPanelOpen ? ' active' : ''}`} type="button" onClick={onToggleOverview} title="Conversation 概览">
          <Monitor size={18} />
        </button>
        <button className={`chrome-button${workspacePanelOpen ? ' active' : ''}`} type="button" onClick={onToggleWorkspace} title="工作区文件">
          <Folder size={18} />
        </button>
        <button className={`chrome-button${previewPanelOpen ? ' active' : ''}`} type="button" onClick={onTogglePreview} title="文件预览">
          <FileSearch size={18} />
        </button>
      </div>
    </header>
  );
}

function ProtocolLogButton({ rawRenderMessages = [] }) {
  const [open, setOpen] = useState(false);
  const [records, setRecords] = useState(() => readChatProtocolDiagnostics());
  const [panel, setPanel] = useState('logs');
  const [filters, setFilters] = useState(() => ({
    stream: false,
    append_stream_to_ui: true,
    replace_ui_element: true
  }));
  const [copyState, setCopyState] = useState('');
  const latest = records.at(-1);
  const visibleRecords = useMemo(() => records.filter((record) => logRecordVisible(record, filters)).slice(-220).reverse(), [records, filters]);
  const rawRenderSnapshot = useMemo(() => chatRawRenderSnapshot(rawRenderMessages), [rawRenderMessages]);

  useEffect(() => {
    const onRecord = (event) => {
      setRecords((current) => [...current, event.detail].slice(-600));
    };
    const onClear = () => setRecords([]);
    window.addEventListener('stellacode:protocol-log', onRecord);
    window.addEventListener('stellacode:protocol-log-cleared', onClear);
    return () => {
      window.removeEventListener('stellacode:protocol-log', onRecord);
      window.removeEventListener('stellacode:protocol-log-cleared', onClear);
    };
  }, []);

  useEffect(() => {
    window.__stellacodeProtocolLogOpen = open;
    return () => {
      window.__stellacodeProtocolLogOpen = false;
    };
  }, [open]);

  const copyLogs = async () => {
    try {
      const payload = panel === 'raw'
        ? rawRenderSnapshot
        : visibleRecords.slice().reverse();
      await navigator.clipboard.writeText(JSON.stringify(payload, null, 2));
      setCopyState('已复制');
      setTimeout(() => setCopyState(''), 1200);
    } catch {
      setCopyState('复制失败');
      setTimeout(() => setCopyState(''), 1600);
    }
  };

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button className={`chrome-button protocol-log-button${records.length > 0 ? ' active' : ''}`} type="button" title="全局日志">
          <ScrollText size={17} />
          <span>LOG</span>
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="protocol-log-popover" align="end" sideOffset={8}>
          <div className="protocol-log-header">
            <div>
              <strong>全局日志</strong>
              <span>
                {panel === 'raw'
                  ? `Raw ${rawRenderSnapshot.counts.raw_messages}/${rawRenderSnapshot.counts.display_messages}/${rawRenderSnapshot.counts.render_entries}`
                  : records.length ? `${visibleRecords.length}/${records.length} 条 · 最新 ${shortLogTime(latest?.time)}` : '暂无日志'}
              </span>
            </div>
            <div className="protocol-log-actions">
              {copyState && <em>{copyState}</em>}
              <button type="button" onClick={copyLogs} title="复制日志">
                <Copy size={14} />
              </button>
              <button type="button" onClick={clearChatProtocolDiagnostics} title="清空日志">
                <Trash2 size={14} />
              </button>
            </div>
          </div>
          <div className="protocol-log-tabs" aria-label="日志面板">
            <ProtocolLogFilterButton label="Events" active={panel === 'logs'} onClick={() => setPanel('logs')} />
            <ProtocolLogFilterButton label="Raw Render" active={panel === 'raw'} onClick={() => setPanel('raw')} />
          </div>
          {panel === 'logs' ? (
            <>
              <div className="protocol-log-filters" aria-label="日志类型筛选">
                <ProtocolLogFilterButton label="Stream" active={filters.stream} onClick={() => setFilters((current) => ({ ...current, stream: !current.stream }))} />
                <ProtocolLogFilterButton label="Append Stream to UI" active={filters.append_stream_to_ui} onClick={() => setFilters((current) => ({ ...current, append_stream_to_ui: !current.append_stream_to_ui }))} />
                <ProtocolLogFilterButton label="Replace UI element" active={filters.replace_ui_element} onClick={() => setFilters((current) => ({ ...current, replace_ui_element: !current.replace_ui_element }))} />
              </div>
              <div className="protocol-log-list">
                {visibleRecords.length === 0 && <div className="protocol-log-empty">没有记录到前端协议日志</div>}
                {visibleRecords.map((record, index) => (
                  <ProtocolLogRow record={record} key={`${record.time || 'log'}-${index}`} />
                ))}
              </div>
            </>
          ) : (
            <RawRenderPanel snapshot={rawRenderSnapshot} />
          )}
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

function RawRenderPanel({ snapshot }) {
  const overviewText = chatRenderOverviewText(snapshot);
  return (
    <div className="protocol-log-list raw-render-list">
      <details className="protocol-log-row raw-render-overview" open>
        <summary>
          <span>{shortLogTime(snapshot.generated_at)}</span>
          <strong>overview</strong>
          <em>compact text summary</em>
        </summary>
        <pre>{overviewText}</pre>
      </details>
      <details className="protocol-log-row" open>
        <summary>
          <span>{shortLogTime(snapshot.generated_at)}</span>
          <strong>raw messages</strong>
          <em>{snapshot.counts.raw_messages} items</em>
        </summary>
        <pre>{JSON.stringify(snapshot.raw_messages.slice(-6), null, 2)}</pre>
      </details>
      <details className="protocol-log-row">
        <summary>
          <span>{shortLogTime(snapshot.generated_at)}</span>
          <strong>displayMessages</strong>
          <em>{snapshot.counts.display_messages} items</em>
        </summary>
        <pre>{JSON.stringify(snapshot.display_messages.slice(-6), null, 2)}</pre>
      </details>
      <details className="protocol-log-row">
        <summary>
          <span>{shortLogTime(snapshot.generated_at)}</span>
          <strong>renderEntries</strong>
          <em>{snapshot.counts.render_entries} items</em>
        </summary>
        <pre>{JSON.stringify(snapshot.render_entries.slice(-6), null, 2)}</pre>
      </details>
    </div>
  );
}

function ProtocolLogFilterButton({ label, active, onClick }) {
  return (
    <button className={active ? 'active' : ''} type="button" onClick={onClick}>
      {label}
    </button>
  );
}

function logRecordVisible(record, filters) {
  const category = logRecordCategory(record);
  if (!category) return true;
  return Boolean(filters[category]);
}

function logRecordCategory(record) {
  const explicit = String(record?.category || '').trim();
  if (explicit) return explicit;
  const kind = String(record?.kind || '');
  if (kind === 'chat.stream') return 'stream';
  if (kind === 'chat.append_stream_to_ui') return 'append_stream_to_ui';
  if (kind === 'chat.replace_ui_element' || kind === 'chat.message_arrived') return 'replace_ui_element';
  if (kind.includes('stream_update')) return 'append_stream_to_ui';
  return '';
}

function ProtocolLogRow({ record }) {
  const kind = String(record?.kind || 'log');
  const severity = kind.includes('mismatch') || kind.includes('gap') || kind.includes('error')
    ? 'error'
    : kind.includes('warning')
      ? 'warning'
      : kind.includes('stream_')
        ? 'stream'
        : 'info';
  return (
    <details className={`protocol-log-row ${severity}`}>
      <summary>
        <span>{shortLogTime(record?.time)}</span>
        <strong>{kind}</strong>
        <em>{logRecordSummary(record)}</em>
      </summary>
      <pre>{JSON.stringify(record, null, 2)}</pre>
    </details>
  );
}

function logRecordSummary(record) {
  if (record?.action) {
    const delta = record.delta ? ` · ${record.delta}` : '';
    const message = record.incoming?.[0]?.text || record.afterTail?.at(-1)?.text || '';
    return `${record.action}${delta || (message ? ` · ${message}` : '')}`;
  }
  const payload = record?.payload || record?.event || record?.projection || record?.mismatches;
  if (payload?.type) return String(payload.type);
  if (Array.isArray(payload)) return `${payload.length} items`;
  if (record?.scopeKey) return String(record.scopeKey);
  return '';
}

function shortLogTime(value) {
  if (!value) return '--:--:--';
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return '--:--:--';
  return date.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

function TransferButton({ transfers }) {
  const active = (transfers || []).filter((item) => !item.done);
  const latest = active[0] || (transfers || [])[0];
  const hasTransfers = Boolean(latest);
  const label = latest?.type === 'upload' ? '上传文件' : '下载文件';
  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <button className={`chrome-button transfer-button${hasTransfers ? ' active has-transfer' : ''}`} type="button" title="传输">
          {active.length > 0 ? <span className="transfer-spinner" /> : <Download size={18} />}
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="transfer-popover" align="end" sideOffset={8}>
          <div className="transfer-popover-header">
            <span className={`transfer-status-dot ${latest?.state || 'idle'}`} />
            <strong>{hasTransfers ? label : '传输任务'}</strong>
            <em>{hasTransfers ? latest.detail : '暂无上传或下载任务'}</em>
          </div>
          <div className="transfer-list">
            {(transfers || []).length === 0 && <div className="transfer-empty">没有正在进行的传输</div>}
            {(transfers || []).map((item) => (
              <div className={`transfer-row ${item.state || 'running'}`} key={item.id}>
                <span className="transfer-row-icon">{item.type === 'upload' ? <Upload size={14} /> : <Download size={14} />}</span>
                <div>
                  <strong>{item.title}</strong>
                  <span>{item.detail}</span>
                </div>
                {!item.done && <span className="transfer-spinner" />}
              </div>
            ))}
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
