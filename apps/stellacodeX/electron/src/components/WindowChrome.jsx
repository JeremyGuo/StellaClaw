import { Download, FileSearch, Folder, MessageSquarePlus, Monitor, PanelLeft, TerminalSquare, Upload } from 'lucide-react';
import * as Popover from '@radix-ui/react-popover';

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
