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

function sameStringSet(left, right) {
  if (left.size !== right.size) return false;
  for (const value of left) {
    if (!right.has(value)) return false;
  }
  return true;
}

export function ConversationBar({
  serverId,
  sidebarMode,
  conversations,
  hiddenConversationIds = [],
  conversationOrder = [],
  openConversationIds = [],
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
  onConversationOrderChange,
  onOpenFoldersChange,
  onCreateSession,
  onRenameSession,
  onDeleteSession
}) {
  const [hiddenOpen, setHiddenOpen] = useState(false);
  const [openFolders, setOpenFolders] = useState(() => new Set());
  const [draggingConversationId, setDraggingConversationId] = useState('');
  const [dropTarget, setDropTarget] = useState({ id: '', position: 'before' });
  const hiddenIds = useMemo(() => new Set(hiddenConversationIds.map(String)), [hiddenConversationIds]);
  const visibleConversations = useMemo(() => {
    const visible = conversations.filter((conversation) => !hiddenIds.has(conversation.conversation_id));
    const sourceIndex = new Map(visible.map((conversation, index) => [conversation.conversation_id, index]));
    const rank = new Map(conversationOrder.map((conversationId, index) => [String(conversationId), index]));
    return [...visible].sort((left, right) => {
      const leftRank = rank.has(left.conversation_id) ? rank.get(left.conversation_id) : Number.MAX_SAFE_INTEGER;
      const rightRank = rank.has(right.conversation_id) ? rank.get(right.conversation_id) : Number.MAX_SAFE_INTEGER;
      if (leftRank !== rightRank) return leftRank - rightRank;
      return (sourceIndex.get(left.conversation_id) || 0) - (sourceIndex.get(right.conversation_id) || 0);
    });
  }, [conversations, conversationOrder, hiddenIds]);
  const hiddenConversations = useMemo(
    () => conversations.filter((conversation) => hiddenIds.has(conversation.conversation_id)),
    [conversations, hiddenIds]
  );

  useEffect(() => {
    setOpenFolders(new Set(openConversationIds.map(String)));
  }, [openConversationIds]);

  const updateOpenFolders = (updater) => {
    setOpenFolders((current) => {
      const next = updater(current);
      if (sameStringSet(current, next)) return current;
      onOpenFoldersChange?.(Array.from(next));
      return next;
    });
  };

  useEffect(() => {
    if (!selected?.conversationId) return;
    updateOpenFolders((current) => {
      if (current.has(selected.conversationId)) return current;
      return new Set(current).add(selected.conversationId);
    });
  }, [selected?.conversationId]);

  const moveConversation = (sourceId, targetId, position = 'before') => {
    if (!sourceId || !targetId || sourceId === targetId) return;
    const ids = visibleConversations.map((conversation) => conversation.conversation_id);
    const sourceIndex = ids.indexOf(sourceId);
    const initialTargetIndex = ids.indexOf(targetId);
    if (sourceIndex < 0 || initialTargetIndex < 0) return;
    ids.splice(sourceIndex, 1);
    const targetIndex = ids.indexOf(targetId);
    if (targetIndex < 0) return;
    ids.splice(targetIndex + (position === 'after' ? 1 : 0), 0, sourceId);
    onConversationOrderChange?.(ids);
  };

  const updateDropTarget = (event, conversationId) => {
    if (!draggingConversationId || draggingConversationId === conversationId) return;
    event.preventDefault();
    const rect = event.currentTarget.getBoundingClientRect();
    const position = event.clientY > rect.top + rect.height / 2 ? 'after' : 'before';
    setDropTarget({ id: conversationId, position });
  };

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
              onSelect={() => onRenameSession?.(conversation, session)}
            >
              重命名 Session
            </ContextMenu.Item>
            {!session?.is_main && (
              <ContextMenu.Item
                className="context-menu-item danger"
                onSelect={() => onDeleteSession?.(conversation, session)}
              >
                删除 Session
              </ContextMenu.Item>
            )}
          </ContextMenu.Content>
        </ContextMenu.Portal>
      </ContextMenu.Root>
    );
  };

  const renderConversation = (conversation, hidden = false) => {
    const sessions = foregroundSessions(conversation);
    const open = openFolders.has(conversation.conversation_id);
    const dragging = draggingConversationId === conversation.conversation_id;
    const dropping = dropTarget.id === conversation.conversation_id && draggingConversationId && draggingConversationId !== conversation.conversation_id;
    return (
      <section
        className={`conversation-folder${dragging ? ' dragging' : ''}${dropping ? ` drop-target drop-${dropTarget.position}` : ''}`}
        key={conversation.conversation_id}
        draggable={!hidden}
        onDragStart={(event) => {
          if (hidden) return;
          setDraggingConversationId(conversation.conversation_id);
          event.dataTransfer.effectAllowed = 'move';
          event.dataTransfer.setData('text/plain', conversation.conversation_id);
        }}
        onDragEnter={(event) => {
          if (hidden) return;
          updateDropTarget(event, conversation.conversation_id);
        }}
        onDragOver={(event) => {
          if (hidden) return;
          updateDropTarget(event, conversation.conversation_id);
          event.dataTransfer.dropEffect = 'move';
        }}
        onDrop={(event) => {
          if (hidden) return;
          event.preventDefault();
          const sourceId = event.dataTransfer.getData('text/plain') || draggingConversationId;
          moveConversation(sourceId, conversation.conversation_id, dropTarget.position);
          setDraggingConversationId('');
          setDropTarget({ id: '', position: 'before' });
        }}
        onDragEnd={() => {
          setDraggingConversationId('');
          setDropTarget({ id: '', position: 'before' });
        }}
      >
        <ContextMenu.Root>
          <ContextMenu.Trigger asChild>
            <button
              className="conversation-folder-row"
              type="button"
              onClick={() => updateOpenFolders((current) => {
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
              <span
                className="conversation-folder-action"
                role="button"
                tabIndex={0}
                aria-label="新建对话"
                onClick={(event) => {
                  event.preventDefault();
                  event.stopPropagation();
                  onCreateSession?.(conversation);
                }}
                onKeyDown={(event) => {
                  if (event.key !== 'Enter' && event.key !== ' ') return;
                  event.preventDefault();
                  event.stopPropagation();
                  onCreateSession?.(conversation);
                }}
              >
                <Plus size={13} />
              </span>
            </button>
          </ContextMenu.Trigger>
          <ContextMenu.Portal>
            <ContextMenu.Content className="context-menu">
              <ContextMenu.Item className="context-menu-item" onSelect={() => onCreateSession?.(conversation)}>
                新建对话
              </ContextMenu.Item>
              <ContextMenu.Item className="context-menu-item" onSelect={() => onRename?.(conversation)}>
                重命名 Conversation
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
