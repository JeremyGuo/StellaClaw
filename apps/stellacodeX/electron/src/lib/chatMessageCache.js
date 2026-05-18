import { messageIndex } from './messageUtils';
import { readLocalCache, removeLocalCache, writeLocalCache } from './localCache';

const MESSAGE_CACHE_LIMIT = 240;

function durableMessagesForCache(messages) {
  return (Array.isArray(messages) ? messages : [])
    .filter((message) => (
      message
      && !message._streaming
      && !message._optimistic
      && !message.pending
      && !message.queued
      && !message._userMessageStarted
    ))
    .sort((left, right) => messageIndex(left) - messageIndex(right))
    .slice(-MESSAGE_CACHE_LIMIT);
}

export function readMessageCache(serverId, conversationId, foregroundSessionId) {
  return readLocalCache('messages', [serverId, conversationId, foregroundSessionId || 'main']) || [];
}

export function writeMessageCache(serverId, conversationId, foregroundSessionId, messages) {
  const durable = durableMessagesForCache(messages);
  if (durable.length > 0) {
    writeLocalCache('messages', [serverId, conversationId, foregroundSessionId || 'main'], durable);
  }
}

export function removeMessageCache(serverId, conversationId, foregroundSessionId) {
  removeLocalCache('messages', [serverId, conversationId, foregroundSessionId || 'main']);
}
