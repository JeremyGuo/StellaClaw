import { ChevronDown, ChevronRight, Folder, Search, Settings } from 'lucide-react';
import { useMemo, useState } from 'react';
import * as ContextMenu from '@radix-ui/react-context-menu';
import { conversationKey, displayConversationName } from '../lib/api';
import { formatModel } from '../lib/format';

function hasUnreadMessage(conversation, active) {
  if (active) return false;
  const lastId = Number(conversation?.last_message_id);
  if (!Number.isFinite(lastId)) return false;
  const seenId = Number(conversation?.last_seen_message_id ?? -1);
  return lastId > seenId;
}

function isWorkingConversation(conversation, activeRunning) {
  const processingState = String(conversation?.processing_state || '').trim().toLowerCase();
  return Boolean(conversation?.running)
    || (processingState && processingState !== 'idle')
    || Boolean(activeRunning);
}

export function ConversationBar({
  serverId,
  sidebarMode,
  conversations,
  hiddenConversationIds = [],
  statuses,
  selected,
  loading,
  activeRunning,
  onSelect,
  onOpenSettings,
  onRename,
  onHide,
  onUnhide,
  onDelete
}) {
  const [hiddenOpen, setHiddenOpen] = useState(false);
  const hiddenIds = useMemo(() => new Set(hiddenConversationIds.map(String)), [hiddenConversationIds]);
  const visibleConversations = useMemo(
    () => conversations.filter((conversation) => !hiddenIds.has(conversation.conversation_id)),
    [conversations, hiddenIds]
  );
  const hiddenConversations = useMemo(
    () => conversations.filter((conversation) => hiddenIds.has(conversation.conversation_id)),
    [conversations, hiddenIds]
  );

  const renderConversation = (conversation, hidden = false) => {
    const key = conversationKey(serverId, conversation.conversation_id);
    const active = selected?.conversationId === conversation.conversation_id;
    const status = statuses.get(key);
    const unread = hasUnreadMessage(conversation, active);
    const working = isWorkingConversation(conversation, active && activeRunning);
    return (
      <ContextMenu.Root key={conversation.conversation_id}>
        <ContextMenu.Trigger asChild>
          <button
            className={`conversation-row${active ? ' active' : ''}${unread ? ' unread' : ''}${working ? ' working' : ''}${hidden ? ' hidden' : ''}`}
            type="button"
            onClick={() => onSelect({ serverId, conversationId: conversation.conversation_id })}
          >
            {unread && <i className="conversation-unread-dot" aria-label="有新消息" />}
            <strong>{displayConversationName(conversation)}</strong>
            <span>{conversation.nickname || conversation.platform_chat_id || 'Local Stellaclaw'}</span>
            <em title={working ? '正在工作' : undefined}>
              {working && <i className="conversation-working-dot" aria-hidden="true" />}
              {formatModel(conversation, status)}
            </em>
          </button>
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Content className="context-menu">
            <ContextMenu.Item
              className="context-menu-item"
              onSelect={() => onRename?.(conversation)}
            >
              重命名
            </ContextMenu.Item>
            <ContextMenu.Item
              className="context-menu-item"
              onSelect={() => (hidden ? onUnhide?.(conversation) : onHide?.(conversation))}
            >
              {hidden ? '取消隐藏' : '隐藏'}
            </ContextMenu.Item>
            <ContextMenu.Item
              className="context-menu-item danger"
              onSelect={() => onDelete?.(conversation)}
            >
              删除
            </ContextMenu.Item>
          </ContextMenu.Content>
        </ContextMenu.Portal>
      </ContextMenu.Root>
    );
  };

  return (
    <aside className="conversation-bar">
      <div className="conversation-top-spacer" />
      <nav className="nav-stack">
        <button className="nav-row" type="button">
          <Search size={18} />
          <span>搜索</span>
        </button>
        <button className="nav-row" type="button" onClick={onOpenSettings}>
          <Settings size={18} />
          <span>设置</span>
        </button>
      </nav>
      <div className="sidebar-label">
        <span>Conversations</span>
        {loading && <span className="sidebar-spinner" aria-label="正在刷新" />}
      </div>
      <div className="conversation-list">
        {visibleConversations.map((conversation) => renderConversation(conversation))}
        {hiddenConversations.length > 0 && (
          <section className="conversation-folder">
            <button
              className="conversation-folder-row"
              type="button"
              onClick={() => setHiddenOpen((value) => !value)}
              aria-expanded={hiddenOpen}
            >
              {hiddenOpen ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
              <Folder size={14} />
              <span>已隐藏</span>
              <em>{hiddenConversations.length}</em>
            </button>
            {hiddenOpen && (
              <div className="conversation-folder-list">
                {hiddenConversations.map((conversation) => renderConversation(conversation, true))}
              </div>
            )}
          </section>
        )}
      </div>
      <div className="sidebar-footer-note">{sidebarMode === 'collapsed' ? '' : 'stellacodex'}</div>
    </aside>
  );
}
