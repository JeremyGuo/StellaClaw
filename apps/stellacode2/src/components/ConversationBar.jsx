import { Search, Settings } from 'lucide-react';
import * as ContextMenu from '@radix-ui/react-context-menu';
import { conversationKey, displayConversationName } from '../lib/api';
import { formatModel } from '../lib/format';

function hasUnreadMessage(settings, key, conversation, active) {
  if (active) return false;
  const lastId = Number(conversation?.last_message_id);
  if (!Number.isFinite(lastId)) return false;
  const seenId = Number(
    conversation?.last_seen_message_id
    ?? settings?.conversationRead?.[key]?.lastSeenMessageId
    ?? -1
  );
  return lastId > seenId;
}

export function ConversationBar({ settings, serverId, sidebarMode, conversations, statuses, selected, loading, onSelect, onOpenSettings, onRename, onDelete }) {
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
        {conversations.map((conversation) => {
          const key = conversationKey(serverId, conversation.conversation_id);
          const active = selected?.conversationId === conversation.conversation_id;
          const status = statuses.get(key);
          const unread = hasUnreadMessage(settings, key, conversation, active);
          return (
            <ContextMenu.Root key={conversation.conversation_id}>
              <ContextMenu.Trigger asChild>
                <button
                  className={`conversation-row${active ? ' active' : ''}${unread ? ' unread' : ''}`}
                  type="button"
                  onClick={() => onSelect({ serverId, conversationId: conversation.conversation_id })}
                >
                  {unread && <i className="conversation-unread-dot" aria-label="有新消息" />}
                  <strong>{displayConversationName(settings, serverId, conversation)}</strong>
                  <span>{conversation.nickname || conversation.platform_chat_id || 'Local Stellaclaw'}</span>
                  <em>{formatModel(conversation, status)}</em>
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
                    className="context-menu-item danger"
                    onSelect={() => onDelete?.(conversation)}
                  >
                    删除
                  </ContextMenu.Item>
                </ContextMenu.Content>
              </ContextMenu.Portal>
            </ContextMenu.Root>
          );
        })}
      </div>
      <div className="sidebar-footer-note">{sidebarMode === 'collapsed' ? '' : 'Stellacode 2'}</div>
    </aside>
  );
}
