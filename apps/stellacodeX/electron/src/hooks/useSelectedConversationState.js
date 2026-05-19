import { useMemo } from 'react';
import { conversationKey, foregroundSessions } from '../lib/api';
import { statusUsageTotals } from '../lib/format';
import { chatSessionStateIsActive, isActiveSessionState } from '../lib/chatSessionState';

const IDLE_CHAT_SESSION_STATE = Object.freeze({ state: 'idle' });

export function useSelectedConversationState({
  selected,
  selectedSessionId,
  conversations,
  propertiesConversation,
  statuses,
  chatSessionState,
  statusDeltas
}) {
  const activeConversation = useMemo(
    () => conversations.find((item) => item.conversation_id === selected?.conversationId) || null,
    [conversations, selected?.conversationId]
  );
  const propertiesConversationCurrent = useMemo(() => (
    propertiesConversation
      ? conversations.find((item) => item.conversation_id === propertiesConversation.conversation_id) || propertiesConversation
      : null
  ), [conversations, propertiesConversation]);
  const activeForegroundSession = useMemo(() => {
    if (!activeConversation) return null;
    const sessions = foregroundSessions(activeConversation);
    return sessions.find((session) => (
      String(session?.id || 'main') === selectedSessionId
    )) || sessions[0] || null;
  }, [activeConversation, selectedSessionId]);
  const selectedKey = selected ? conversationKey(selected.serverId, selected.conversationId, selectedSessionId) : '';
  const selectedConversationUiKey = selectedKey;
  const legacyConversationUiKey = selected ? conversationKey(selected.serverId, selected.conversationId, 'main') : '';
  const selectedStatus = selected ? statuses.get(selectedKey) : null;
  const selectedChatSessionState = chatSessionState.scopeKey === selectedKey
    ? chatSessionState
    : IDLE_CHAT_SESSION_STATE;
  const selectedConversationStatus = useMemo(() => ({
    ...(selectedStatus || {}),
    ...(activeConversation ? {
      model: activeConversation.model,
      model_selection_pending: activeConversation.model_selection_pending,
      reasoning: activeConversation.reasoning,
      sandbox: activeConversation.sandbox,
      sandbox_source: activeConversation.sandbox_source,
      remote: activeConversation.remote,
      workspace: activeConversation.workspace,
      processing_state: selectedChatSessionState.state || 'idle',
      running: chatSessionStateIsActive(selectedChatSessionState),
      running_background: activeConversation.running_background,
      total_background: activeConversation.total_background,
      running_subagents: activeConversation.running_subagents,
      total_subagents: activeConversation.total_subagents
    } : {})
  }), [selectedStatus, activeConversation, selectedChatSessionState]);
  const selectedUsage = useMemo(
    () => statusUsageTotals(selectedStatus, selectedKey ? statusDeltas.get(selectedKey) : null),
    [selectedStatus, selectedKey, statusDeltas]
  );
  const selectedProcessingState = String(selectedConversationStatus?.processing_state || '').trim().toLowerCase();
  const selectedProcessing = Boolean(selectedConversationStatus?.running)
    || isActiveSessionState(selectedProcessingState);

  return {
    activeConversation,
    propertiesConversationCurrent,
    activeForegroundSession,
    selectedKey,
    selectedConversationUiKey,
    legacyConversationUiKey,
    selectedStatus,
    selectedConversationStatus,
    selectedUsage,
    selectedProcessing
  };
}
