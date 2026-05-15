import { ChevronDown, ChevronRight, Folder, Plus, Search, Settings } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import * as ContextMenu from '@radix-ui/react-context-menu';
import { conversationKey, displayConversationName, displayForegroundSessionName, foregroundSessions } from '../lib/api';
import { formatModel } from '../lib/format';
import { messageOrderFromId } from '../lib/messageUtils';

function hasUnreadMessage(session, active) {
  if (active) return false;
  const lastId = messageOrderFromId(session?.last_message_id);
  if (lastId === undefined) return false;
  const seenId = messageOrderFromId(session?.last_seen_message_id) ?? -1;
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
  onDelete,
  onCreateSession,
  onDeleteSession
}) {
  const [hiddenOpen, setHiddenOpen] = useState(false);
  const [openFolders, setOpenFolders] = useState(() => new Set());
  const hiddenIds = useMemo(() => new Set(hiddenConversationIds.map(String)), [hiddenConversationIds]);
  const visibleConversations = useMemo(
    () => conversations.filter((conversation) => !hiddenIds.has(conversation.conversation_id)),
    [conversations, hiddenIds]
  );
  const hiddenConversations = useMemo(
    () => conversations.filter((conversation) => hiddenIds.has(conversation.conversation_id)),
    [conversations, hiddenIds]
  );

  useEffect(() => {
    if (!selected?.conversationId) return;
    setOpenFolders((current) => new Set(current).add(selected.conversationId));
  }, [selected?.conversationId]);

  const renderSession = (conversation, session, hidden = false) => {
    const sessionId = session?.id || 'main';
    const key = conversationKey(serverId, conversation.conversation_id, sessionId);
    const active = selected?.conversationId === conversation.conversation_id
      && (selected?.foregroundSessionId || 'main') === sessionId;
    const status = statuses.get(key) || statuses.get(conversationKey(serverId, conversation.conversation_id, 'main'));
    const unread = hasUnreadMessage(session, active);
    const working = isWorkingConversation(conversation, active && activeRunning);
    const title = displayForegroundSessionName(session, conversation);
    return (
      <ContextMenu.Root key={`${conversation.conversation_id}:${sessionId}`}>
        <ContextMenu.Trigger asChild>
          <button
            className={`conversation-row session-row${active ? ' active' : ''}${unread ? ' unread' : ''}${working ? ' working' : ''}${hidden ? ' hidden' : ''}`}
            type="button"
            onClick={() => onSelect({ serverId, conversationId: conversation.conversation_id, foregroundSessionId: sessionId })}
          >
            {unread && <i className="conversation-unread-dot" aria-label="有新消息" />}
            <strong>{title}</strong>
            <span>{session?.is_main ? 'Main' : (conversation.nickname || conversation.platform_chat_id || 'Foreground')}</span>
            <em title={working ? '正在工作' : undefined}>
              {working && <i className="conversation-working-dot" aria-hidden="true" />}
              {session?.last_message_time ? relativeSessionTime(session.last_message_time) : formatModel(conversation, status)}
            </em>
          </button>
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Content className="context-menu">
            <ContextMenu.Item
              className="context-menu-item"
              onSelect={() => onRename?.(conversation)}
            >
              重命名 Conversation
            </ContextMenu.Item>
            {!session?.is_main && (
              <ContextMenu.Item
                className="context-menu-item danger"
                onSelect={() => onDeleteSession?.(conversation, session)}
              >
                删除对话
              </ContextMenu.Item>
            )}
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

  const renderConversation = (conversation, hidden = false) => {
    const sessions = foregroundSessions(conversation);
    const open = openFolders.has(conversation.conversation_id)
      || selected?.conversationId === conversation.conversation_id
      || sessions.length <= 5;
    const unreadCount = sessions.filter((session) => hasUnreadMessage(session, selected?.conversationId === conversation.conversation_id && (selected?.foregroundSessionId || 'main') === session.id)).length;
    return (
      <section className="conversation-folder" key={conversation.conversation_id}>
        <ContextMenu.Root>
          <ContextMenu.Trigger asChild>
            <button
              className="conversation-folder-row"
              type="button"
              onClick={() => setOpenFolders((current) => {
                const next = new Set(current);
                if (next.has(conversation.conversation_id)) next.delete(conversation.conversation_id);
                else next.add(conversation.conversation_id);
                return next;
              })}
              aria-expanded={open}
            >
              {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
              <Folder size={15} />
              <span>{displayConversationName(conversation)}</span>
              <em>{unreadCount > 0 ? unreadCount : sessions.length}</em>
            </button>
          </ContextMenu.Trigger>
          <ContextMenu.Portal>
            <ContextMenu.Content className="context-menu">
              <ContextMenu.Item className="context-menu-item" onSelect={() => onCreateSession?.(conversation)}>
                新建对话
              </ContextMenu.Item>
              <ContextMenu.Item className="context-menu-item" onSelect={() => onRename?.(conversation)}>
                重命名文件夹
              </ContextMenu.Item>
              <ContextMenu.Item className="context-menu-item" onSelect={() => (hidden ? onUnhide?.(conversation) : onHide?.(conversation))}>
                {hidden ? '取消隐藏' : '隐藏'}
              </ContextMenu.Item>
              <ContextMenu.Item className="context-menu-item danger" onSelect={() => onDelete?.(conversation)}>
                删除 Conversation
              </ContextMenu.Item>
            </ContextMenu.Content>
          </ContextMenu.Portal>
        </ContextMenu.Root>
        {open && (
          <div className="conversation-folder-list">
            {sessions.map((session) => renderSession(conversation, session, hidden))}
            <button className="conversation-add-session" type="button" onClick={() => onCreateSession?.(conversation)}>
              <Plus size={13} />
              <span>新建对话</span>
            </button>
          </div>
        )}
      </section>
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

function relativeSessionTime(value) {
  const time = Date.parse(value);
  if (!Number.isFinite(time)) return '';
  const minutes = Math.max(0, Math.round((Date.now() - time) / 60_000));
  if (minutes < 60) return `${Math.max(1, minutes)} 分`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} 小时`;
  return `${Math.round(hours / 24)} 天`;
}
