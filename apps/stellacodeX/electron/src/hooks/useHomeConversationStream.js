import { useEffect } from 'react';
import { conversationStreamUrl, displayConversationName, foregroundSessions } from '../lib/api';
import { applyConversationStreamEvent, hasUnreadConversation } from '../lib/conversationState';

function homeEventType(payload) {
  const type = String(payload?.type || '');
  return type.startsWith('home.') ? type.slice('home.'.length) : type;
}

function firstSessionId(conversation) {
  return conversation ? foregroundSessions(conversation)[0]?.id || 'main' : 'main';
}

export function useHomeConversationStream({
  activeServerId,
  settingsReady,
  conversationsRef,
  selectedRef,
  appForegroundRef,
  setConversations,
  setSelected
}) {
  useEffect(() => {
    if (!activeServerId || !settingsReady) return undefined;
    let disposed = false;
    let reconnectTimer = null;
    let streamSocket = null;
    const connect = async () => {
      try {
        const url = await conversationStreamUrl(activeServerId);
        if (disposed) return;
        const socket = new WebSocket(url);
        streamSocket = socket;
        socket.addEventListener('message', (event) => {
          let payload;
          try {
            payload = JSON.parse(event.data);
          } catch {
            return;
          }
          const currentConversations = conversationsRef.current;
          const nextConversations = applyConversationStreamEvent(currentConversations, payload);
          if (nextConversations !== currentConversations) {
            conversationsRef.current = nextConversations;
            setConversations(nextConversations);
          }
          const type = homeEventType(payload);
          if (
            type === 'conversation_deleted'
            && selectedRef.current?.serverId === activeServerId
            && selectedRef.current?.conversationId === payload.conversation_id
          ) {
            const next = nextConversations[0];
            setSelected(next ? {
              serverId: activeServerId,
              conversationId: next.conversation_id,
              foregroundSessionId: firstSessionId(next)
            } : null);
          }
          if (!selectedRef.current) {
            const fallbackConversation = (type === 'snapshot' || type === 'conversation_snapshot')
              ? (payload.conversations || [])[0]
              : payload.conversation;
            if (fallbackConversation?.conversation_id) {
              setSelected({
                serverId: activeServerId,
                conversationId: fallbackConversation.conversation_id,
                foregroundSessionId: firstSessionId(fallbackConversation)
              });
            }
          }
          if (type === 'conversation_turn_completed' && payload.conversation_id) {
            const completed = nextConversations.find((conversation) => conversation.conversation_id === payload.conversation_id);
            const selectedConversation = selectedRef.current;
            const isActive = selectedConversation?.serverId === activeServerId
              && selectedConversation?.conversationId === payload.conversation_id;
            const isVisibleActive = isActive && appForegroundRef.current;
            if (selectedConversation && completed && !isVisibleActive && hasUnreadConversation(completed)) {
              window.stellacode2?.notify?.({
                title: displayConversationName(completed),
                body: '新回复已完成'
              }).catch(() => {});
            }
          }
        });
        socket.addEventListener('close', () => {
          if (disposed) return;
          reconnectTimer = window.setTimeout(connect, 1600);
        });
        socket.addEventListener('error', () => {});
      } catch {
        if (disposed) return;
        reconnectTimer = window.setTimeout(connect, 2400);
      }
    };
    connect();
    return () => {
      disposed = true;
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      if (streamSocket && streamSocket.readyState <= WebSocket.OPEN) streamSocket.close();
    };
  }, [activeServerId, settingsReady, conversationsRef, selectedRef, appForegroundRef, setConversations, setSelected]);
}
