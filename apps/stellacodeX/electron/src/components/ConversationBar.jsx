import { Folder, FolderOpen, Plus, Search, Settings } from 'lucide-react';
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
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

function deferContextMenuAction(action) {
  window.setTimeout(() => action?.(), 0);
}

function sameStringSet(left, right) {
  if (left.size !== right.size) return false;
  for (const value of left) {
    if (!right.has(value)) return false;
  }
  return true;
}

function sameStringArray(left, right) {
  if (left.length !== right.length) return false;
  return left.every((value, index) => value === right[index]);
}

function reorderedIds(ids, sourceId, targetId, position = 'before') {
  if (!sourceId || !targetId || sourceId === targetId) return ids;
  const next = [...ids];
  const sourceIndex = next.indexOf(sourceId);
  if (sourceIndex < 0 || !next.includes(targetId)) return ids;
  next.splice(sourceIndex, 1);
  const targetIndex = next.indexOf(targetId);
  if (targetIndex < 0) return ids;
  next.splice(targetIndex + (position === 'after' ? 1 : 0), 0, sourceId);
  return sameStringArray(ids, next) ? ids : next;
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
  const [previewConversationIds, setPreviewConversationIds] = useState([]);
  const draggingConversationIdRef = useRef('');
  const previewConversationIdsRef = useRef([]);
  const folderRefs = useRef(new Map());
  const previousFolderRects = useRef(null);
  const dragCommittedRef = useRef(false);
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
  const visibleConversationIds = useMemo(
    () => visibleConversations.map((conversation) => conversation.conversation_id),
    [visibleConversations]
  );
  const displayedConversations = useMemo(() => {
    if (!previewConversationIds.length) return visibleConversations;
    const byId = new Map(visibleConversations.map((conversation) => [conversation.conversation_id, conversation]));
    const ordered = previewConversationIds.map((conversationId) => byId.get(conversationId)).filter(Boolean);
    const orderedIds = new Set(ordered.map((conversation) => conversation.conversation_id));
    return ordered.concat(visibleConversations.filter((conversation) => !orderedIds.has(conversation.conversation_id)));
  }, [previewConversationIds, visibleConversations]);
  const hiddenConversations = useMemo(
    () => conversations.filter((conversation) => hiddenIds.has(conversation.conversation_id)),
    [conversations, hiddenIds]
  );

  useLayoutEffect(() => {
    const previous = previousFolderRects.current;
    if (!previous) return;
    previousFolderRects.current = null;
    folderRefs.current.forEach((node, conversationId) => {
      const before = previous.get(conversationId);
      if (!before) return;
      const after = node.getBoundingClientRect();
      const deltaY = before.top - after.top;
      if (Math.abs(deltaY) < 1) return;
      node.style.transition = 'none';
      node.style.transform = `translateY(${deltaY}px)`;
      node.getBoundingClientRect();
      window.requestAnimationFrame(() => {
        node.style.transition = 'transform 170ms cubic-bezier(.2, .8, .2, 1)';
        node.style.transform = '';
      });
    });
  }, [displayedConversations]);

  useEffect(() => {
    setOpenFolders(new Set(openConversationIds.map(String)));
  }, [openConversationIds]);

  useEffect(() => {
    if (previewConversationIds.length && sameStringArray(previewConversationIds, visibleConversationIds)) {
      setPreviewOrder([]);
    }
  }, [previewConversationIds, visibleConversationIds]);

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

  const setFolderRef = (conversationId) => (node) => {
    if (node) folderRefs.current.set(conversationId, node);
    else folderRefs.current.delete(conversationId);
  };

  const captureFolderRects = () => {
    const rects = new Map();
    folderRefs.current.forEach((node, conversationId) => {
      rects.set(conversationId, node.getBoundingClientRect());
    });
    return rects;
  };

  const animateConversationOrderChange = (updater) => {
    previousFolderRects.current = captureFolderRects();
    updater();
  };

  const setPreviewOrder = (ids) => {
    previewConversationIdsRef.current = ids;
    setPreviewConversationIds(ids);
  };

  const previewMoveConversation = (event, conversationId) => {
    const sourceId = draggingConversationIdRef.current || draggingConversationId;
    if (!sourceId || sourceId === conversationId) return;
    event.preventDefault();
    const rect = event.currentTarget.getBoundingClientRect();
    const position = event.clientY > rect.top + rect.height / 2 ? 'after' : 'before';
    const sourceIds = previewConversationIdsRef.current.length ? previewConversationIdsRef.current : visibleConversationIds;
    const nextIds = reorderedIds(sourceIds, sourceId, conversationId, position);
    if (nextIds === sourceIds) return;
    animateConversationOrderChange(() => setPreviewOrder(nextIds));
  };

  const finishDrag = () => {
    draggingConversationIdRef.current = '';
    setDraggingConversationId('');
    if (!dragCommittedRef.current && previewConversationIds.length) {
      animateConversationOrderChange(() => setPreviewOrder([]));
    }
  };

  const renderSession = (conversation, session, hidden = false) => {
    const sessionId = session?.id || 'main';
    const canDeleteSession = sessionId !== 'main';
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
              onSelect={() => deferContextMenuAction(() => onRenameSession?.(conversation, session))}
            >
              重命名 Session
            </ContextMenu.Item>
            <ContextMenu.Item
              className="context-menu-item danger"
              disabled={!canDeleteSession}
              onSelect={() => {
                if (canDeleteSession) deferContextMenuAction(() => onDeleteSession?.(conversation, session));
              }}
            >
              {canDeleteSession ? '删除 Session' : 'Main Session 不能删除'}
            </ContextMenu.Item>
          </ContextMenu.Content>
        </ContextMenu.Portal>
      </ContextMenu.Root>
    );
  };

  const renderConversation = (conversation, hidden = false) => {
    const sessions = foregroundSessions(conversation);
    const open = openFolders.has(conversation.conversation_id);
    const dragging = draggingConversationId === conversation.conversation_id;
    return (
      <section
        className={`conversation-folder${dragging ? ' dragging' : ''}`}
        key={conversation.conversation_id}
        ref={hidden ? undefined : setFolderRef(conversation.conversation_id)}
        draggable={!hidden}
        onDragStart={(event) => {
          if (hidden) return;
          dragCommittedRef.current = false;
          draggingConversationIdRef.current = conversation.conversation_id;
          setDraggingConversationId(conversation.conversation_id);
          setPreviewOrder(visibleConversationIds);
          event.dataTransfer.effectAllowed = 'move';
          event.dataTransfer.setData('text/plain', conversation.conversation_id);
        }}
        onDragEnter={(event) => {
          if (hidden) return;
          previewMoveConversation(event, conversation.conversation_id);
        }}
        onDragOver={(event) => {
          if (hidden) return;
          previewMoveConversation(event, conversation.conversation_id);
          event.dataTransfer.dropEffect = 'move';
        }}
        onDrop={(event) => {
          if (hidden) return;
          event.preventDefault();
          dragCommittedRef.current = true;
          const nextIds = previewConversationIdsRef.current.length ? previewConversationIdsRef.current : visibleConversationIds;
          onConversationOrderChange?.(nextIds);
          draggingConversationIdRef.current = '';
          setDraggingConversationId('');
        }}
        onDragEnd={finishDrag}
      >
        <ContextMenu.Root>
          <ContextMenu.Trigger asChild>
            <button
              className={`conversation-folder-row${open ? ' open' : ''}`}
              type="button"
              onClick={() => updateOpenFolders((current) => {
                const next = new Set(current);
                if (next.has(conversation.conversation_id)) next.delete(conversation.conversation_id);
                else next.add(conversation.conversation_id);
                return next;
              })}
              aria-expanded={open}
            >
              {open
                ? <FolderOpen className="conversation-folder-icon" size={16} strokeWidth={1.8} />
                : <Folder className="conversation-folder-icon" size={16} strokeWidth={1.8} />}
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
              <ContextMenu.Item className="context-menu-item" onSelect={() => deferContextMenuAction(() => onCreateSession?.(conversation))}>
                新建对话
              </ContextMenu.Item>
              <ContextMenu.Item className="context-menu-item" onSelect={() => deferContextMenuAction(() => onRename?.(conversation))}>
                重命名 Conversation
              </ContextMenu.Item>
              <ContextMenu.Item className="context-menu-item" onSelect={() => deferContextMenuAction(() => (hidden ? onUnhide?.(conversation) : onHide?.(conversation)))}>
                {hidden ? '取消隐藏' : '隐藏'}
              </ContextMenu.Item>
              <ContextMenu.Item className="context-menu-item danger" onSelect={() => deferContextMenuAction(() => onDelete?.(conversation))}>
                删除 Conversation
              </ContextMenu.Item>
            </ContextMenu.Content>
          </ContextMenu.Portal>
        </ContextMenu.Root>
        <div className={`conversation-folder-list${open ? ' open' : ''}`} aria-hidden={!open}>
          <div className="conversation-folder-list-inner">
            {sessions.map((session) => renderSession(conversation, session, hidden))}
          </div>
        </div>
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
        {displayedConversations.map((conversation) => renderConversation(conversation))}
        {hiddenConversations.length > 0 && (
          <section className="conversation-folder">
            <button
              className={`conversation-folder-row${hiddenOpen ? ' open' : ''}`}
              type="button"
              onClick={() => setHiddenOpen((value) => !value)}
              aria-expanded={hiddenOpen}
            >
              {hiddenOpen
                ? <FolderOpen className="conversation-folder-icon" size={16} strokeWidth={1.8} />
                : <Folder className="conversation-folder-icon" size={16} strokeWidth={1.8} />}
              <span>已隐藏</span>
              <em>{hiddenConversations.length}</em>
            </button>
            <div className={`conversation-folder-list${hiddenOpen ? ' open' : ''}`} aria-hidden={!hiddenOpen}>
              <div className="conversation-folder-list-inner">
                {hiddenConversations.map((conversation) => renderConversation(conversation, true))}
              </div>
            </div>
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
