import {
  displayForegroundSessionName,
  foregroundSessions,
  normalizeConversationSummary,
  normalizeForegroundSessionSummary
} from './api';
import { messageOrderFromId } from './messageUtils';

function maxMessageId(...values) {
  let best;
  let bestOrder = -1;
  for (const value of values) {
    const order = messageOrderFromId(value);
    if (order !== undefined && order >= bestOrder) {
      best = String(value);
      bestOrder = order;
    }
  }
  return best;
}

function compareMessageIds(left, right) {
  const leftOrder = messageOrderFromId(left);
  const rightOrder = messageOrderFromId(right);
  if (leftOrder === undefined && rightOrder === undefined) return 0;
  if (leftOrder === undefined) return -1;
  if (rightOrder === undefined) return 1;
  return leftOrder === rightOrder ? 0 : leftOrder > rightOrder ? 1 : -1;
}

function mergeConversationSummary(existing, incoming) {
  incoming = normalizeConversationSummary(incoming);
  if (!existing) return incoming;
  if (!incoming) return existing;
  existing = normalizeConversationSummary(existing);
  const incomingHasNewerMessage = compareMessageIds(incoming.last_message_id, existing.last_message_id) >= 0;
  const seen = maxMessageId(existing.last_seen_message_id, incoming.last_seen_message_id);
  const incomingSeenIsNewer = compareMessageIds(incoming?.last_seen_message_id, existing?.last_seen_message_id) >= 0;
  const merged = {
    ...existing,
    ...incoming,
    last_message_id: incomingHasNewerMessage
      ? incoming.last_message_id ?? existing.last_message_id
      : existing.last_message_id,
    last_message_time: incomingHasNewerMessage
      ? incoming.last_message_time ?? existing.last_message_time
      : existing.last_message_time,
    message_count: incomingHasNewerMessage
      ? incoming.message_count ?? existing.message_count
      : existing.message_count
  };
  if (!seen) return merged;
  return {
    ...merged,
    last_seen_message_id: seen,
    last_seen_at: incomingSeenIsNewer
      ? incoming?.last_seen_at
      : existing?.last_seen_at
  };
}

export function patchConversationForegroundSession(conversation, sessionId, patch) {
  const targetSessionId = String(sessionId || 'main');
  const sessions = foregroundSessions(conversation);
  let found = false;
  const nextSessions = sessions.map((session) => {
    const currentId = String(session?.id || 'main');
    if (currentId !== targetSessionId) return session;
    found = true;
    return normalizeForegroundSessionSummary({ ...session, ...patch, id: currentId }, conversation);
  });
  if (!found) {
    nextSessions.push(normalizeForegroundSessionSummary({
      id: targetSessionId,
      session_id: targetSessionId,
      is_main: targetSessionId === 'main',
      ...patch
    }, conversation));
  }
  return {
    ...conversation,
    ...(targetSessionId === 'main' ? patch : {}),
    foreground_sessions: nextSessions
  };
}

export function nextForegroundSessionName(conversation) {
  const existingNames = new Set(
    foregroundSessions(conversation)
      .map((session) => displayForegroundSessionName(session, conversation).trim())
      .filter(Boolean)
  );
  let index = Math.max(2, existingNames.size + 1);
  while (existingNames.has(`Session ${index}`)) index += 1;
  return `Session ${index}`;
}

export function createLocalForegroundSessionId(conversation) {
  const existingIds = new Set(foregroundSessions(conversation).map((session) => String(session?.id || 'main')));
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const suffix = Math.random().toString(36).slice(2, 8);
    const id = `session_${Date.now().toString(36)}_${suffix}`;
    if (!existingIds.has(id)) return id;
  }
  return `session_${Date.now().toString(36)}`;
}

export function applyConversationStreamEvent(current, payload) {
  const type = String(payload?.type || '');
  const eventType = type.startsWith('home.') ? type.slice('home.'.length) : type;
  const sort = (list) => [...list].sort((left, right) => left.conversation_id.localeCompare(right.conversation_id));
  const upsert = (list, incoming) => {
    if (!incoming?.conversation_id) return list;
    const exists = list.some((conversation) => conversation.conversation_id === incoming.conversation_id);
    if (!exists) return sort([...list, incoming]);
    return list.map((conversation) => (
      conversation.conversation_id === incoming.conversation_id
        ? mergeConversationSummary(conversation, incoming)
        : conversation
    ));
  };

  if (eventType === 'snapshot' || eventType === 'conversation_snapshot') {
    const existingById = new Map(current.map((conversation) => [conversation.conversation_id, conversation]));
    return (payload.conversations || [])
      .map((conversation) => mergeConversationSummary(existingById.get(conversation.conversation_id), conversation));
  }

  if (eventType === 'conversation_upserted') {
    return upsert(current, payload.conversation);
  }

  if (eventType === 'conversation_updated' && payload.conversation_id) {
    return current.map((conversation) => (
      conversation.conversation_id === payload.conversation_id
        ? normalizeConversationSummary({
          ...conversation,
          ...(payload.patch || {}),
          conversation_id: payload.conversation_id
        })
        : conversation
    ));
  }

  if (eventType === 'foreground_session_upserted' && payload.conversation_id && payload.foreground_session) {
    const session = payload.foreground_session;
    const sessionId = session.id || session.foreground_session_id || 'main';
    return current.map((conversation) => (
      conversation.conversation_id === payload.conversation_id
        ? patchConversationForegroundSession(conversation, sessionId, session)
        : conversation
    ));
  }

  if (eventType === 'foreground_session_updated' && payload.conversation_id) {
    const foregroundSessionId = payload.foreground_session_id || payload.session_id || 'main';
    const patch = payload.patch?.foreground_session || payload.patch || {};
    return current.map((conversation) => (
      conversation.conversation_id === payload.conversation_id
        ? patchConversationForegroundSession(conversation, foregroundSessionId, patch)
        : conversation
    ));
  }

  if (eventType === 'foreground_session_deleted' && payload.conversation_id) {
    const foregroundSessionId = String(payload.foreground_session_id || 'main');
    return current.map((conversation) => {
      if (conversation.conversation_id !== payload.conversation_id) return conversation;
      return {
        ...conversation,
        foreground_sessions: foregroundSessions(conversation)
          .filter((session) => String(session?.id || 'main') !== foregroundSessionId)
      };
    });
  }

  if (eventType === 'conversation_deleted' && payload.conversation_id) {
    return current.filter((conversation) => conversation.conversation_id !== payload.conversation_id);
  }

  if (eventType === 'conversation_processing' && payload.conversation_id) {
    return current.map((conversation) => (
      conversation.conversation_id === payload.conversation_id
        ? {
          ...conversation,
          processing_state: payload.processing_state || conversation.processing_state,
          running: Boolean(payload.running)
        }
        : conversation
    ));
  }

  if (eventType === 'conversation_turn_completed' && payload.conversation_id) {
    const incoming = {
      ...(payload.conversation || {}),
      conversation_id: payload.conversation_id,
      platform_chat_id: payload.platform_chat_id || payload.conversation?.platform_chat_id,
      processing_state: 'idle',
      running: false,
      message_count: payload.message_count ?? payload.conversation?.message_count,
      last_message_id: payload.last_message_id ?? payload.conversation?.last_message_id,
      last_message_time: payload.last_message_time ?? payload.conversation?.last_message_time,
      last_seen_message_id: payload.last_seen_message_id ?? payload.conversation?.last_seen_message_id,
      last_seen_at: payload.last_seen_at ?? payload.conversation?.last_seen_at
    };
    return upsert(current, incoming);
  }

  if (eventType === 'foreground_session_state_updated' && payload.conversation_id) {
    const foregroundSessionId = payload.foreground_session_id || 'main';
    const state = String(payload.state || 'idle').toLowerCase();
    const running = state === 'running' || state === 'queued';
    return current.map((conversation) => (
      conversation.conversation_id === payload.conversation_id
        ? patchConversationForegroundSession(conversation, foregroundSessionId, {
          state,
          active_turn_id: payload.active_turn_id || payload.activeTurnId || null,
          last_error: payload.last_error || payload.lastError || null,
          processing_state: state,
          running
        })
        : conversation
    ));
  }

  if (
    (eventType === 'conversation_seen' || eventType === 'foreground_session_seen_state_updated')
    && payload.conversation_id
    && payload.seen
  ) {
    const foregroundSessionId = payload.foreground_session_id || 'main';
    return current.map((conversation) => (
      conversation.conversation_id === payload.conversation_id
        ? patchConversationForegroundSession(conversation, foregroundSessionId, {
          last_seen_message_id: payload.seen.last_seen_message_id,
          last_seen_at: payload.seen.updated_at
        })
        : conversation
    ));
  }

  return current;
}

export function hasUnreadConversation(conversation) {
  return foregroundSessions(conversation).some((session) => (
    compareMessageIds(session?.last_message_id, session?.last_seen_message_id) > 0
  ));
}
