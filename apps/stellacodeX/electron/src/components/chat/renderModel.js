import { displayMessages, isFinalAssistantMessage, messageKey } from '../../lib/messageUtils';

export function buildChatRenderModel({
  messages,
  currentActivity,
  sending = false,
  processing = false,
  modelSelectionPending = false
} = {}) {
  const renderedMessages = displayMessages(messages || []);
  const renderEntries = assistantTurnEntries(renderedMessages);
  const entryKeys = renderEntries.map((entry, index) => chatRenderEntryKey(entry, index));
  const latestAssistantTurnIndex = latestAssistantTurnEntryIndex(renderEntries);
  const pendingAssistantVisible = shouldShowPendingAssistant(renderEntries, currentActivity, sending, processing);
  const responseSpacerVisible = Boolean(pendingAssistantVisible && renderedMessages.length > 0 && !modelSelectionPending);
  return {
    renderedMessages,
    renderEntries,
    entryKeys,
    latestAssistantTurnIndex,
    pendingAssistantVisible,
    responseSpacerVisible
  };
}

export function assistantTurnEntries(renderedMessages) {
  const entries = [];
  for (let index = 0; index < renderedMessages.length; index += 1) {
    const message = renderedMessages[index];
    if (message?.type !== 'toolGroup') {
      entries.push({ type: 'message', id: messageKey(message, index), message });
      continue;
    }
    const nextMessage = renderedMessages[index + 1];
    const finalMessage = isFinalAssistantMessage(nextMessage) ? nextMessage : null;
    entries.push({
      type: 'assistantTurn',
      id: `turn-${message.id || messageKey(message.messages?.[0], index)}`,
      processGroup: message,
      finalMessage
    });
    if (finalMessage) index += 1;
  }
  return entries;
}

export function chatRenderEntryKey(entry, index) {
  if (entry?.type === 'assistantTurn') return entry.id || `assistant-turn-${index}`;
  return messageKey(entry?.message, index);
}

export function latestAssistantTurnEntryIndex(entries) {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    if (entries[index]?.type === 'assistantTurn') return index;
  }
  return -1;
}

export function shouldShowPendingAssistant(entries, currentActivity, sending, processing) {
  const state = String(currentActivity?.state || '').toLowerCase();
  const activityId = String(currentActivity?.id || '').trim();
  const active = Boolean(sending || processing || (currentActivity && state !== 'done' && state !== 'failed'));
  if (!active) return false;
  if (activityId.startsWith('stream-assistant-') || activityId.startsWith('stream-reasoning-') || activityId.startsWith('stream-tool-')) return false;
  const lastUserIndex = findLastEntryIndex(entries, (entry) => (
    entry?.type === 'message' && String(entry.message?.role || '').toLowerCase() === 'user'
  ));
  if (lastUserIndex < 0) return false;
  const hasAssistantAfterUser = entries.slice(lastUserIndex + 1).some((entry) => {
    if (entry?.type === 'assistantTurn') return true;
    if (entry?.type !== 'message') return false;
    return String(entry.message?.role || '').toLowerCase() === 'assistant';
  });
  return !hasAssistantAfterUser;
}

function findLastEntryIndex(entries, predicate) {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    if (predicate(entries[index])) return index;
  }
  return -1;
}
