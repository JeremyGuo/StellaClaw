export function conversationKey(serverId, conversationId, foregroundSessionId = 'main') {
  return `${serverId}:${conversationId}:${foregroundSessionId || 'main'}`;
}

export function normalizeForegroundSessionSummary(session = {}, conversation = {}) {
  const id = String(session.id || session.foreground_session_id || session.session_id || 'main')
    .replace(/^local__agent__foreground__/, '') || 'main';
  const lastMessageId = session.last_message_id || session.last_committed_message_id || null;
  const lastMessageIndex = Number(session.last_message_index ?? session.last_committed_message_index);
  const messageCount = Number(session.message_count);
  const state = String(session.state || session.processing_state || 'idle').toLowerCase();
  return {
    ...session,
    id,
    foreground_session_id: id,
    session_id: session.session_id || `local__agent__foreground__${id}`,
    nickname: session.nickname || session.session_name || (id === 'main' ? displayConversationName(conversation) : id),
    session_name: session.session_name || session.nickname || (id === 'main' ? displayConversationName(conversation) : id),
    is_main: session.is_main ?? id === 'main',
    state,
    processing_state: state,
    running: session.running ?? (state === 'running' || state === 'queued'),
    message_count: Number.isFinite(messageCount)
      ? messageCount
      : Number.isFinite(lastMessageIndex)
        ? lastMessageIndex + 1
        : 0,
    last_message_id: lastMessageId,
    last_message_time: session.last_message_time || session.last_activity_at || session.updated_at || null,
    last_committed_message_id: session.last_committed_message_id || lastMessageId,
    last_committed_message_index: Number.isFinite(lastMessageIndex) ? lastMessageIndex : null,
    last_seen_message_id: session.last_seen_message_id || null,
    last_seen_at: session.last_seen_at || null
  };
}

export function normalizeConversationSummary(conversation = {}) {
  const normalized = {
    ...conversation,
    nickname: conversation.nickname || conversation.conversation_name || conversation.platform_chat_id || conversation.conversation_id,
    conversation_name: conversation.conversation_name || conversation.nickname || conversation.platform_chat_id || conversation.conversation_id,
    last_message_id: conversation.last_message_id || conversation.last_committed_message_id || null,
    last_message_time: conversation.last_message_time || conversation.updated_at || conversation.last_activity_at || null,
    last_committed_message_id: conversation.last_committed_message_id || conversation.last_message_id || null,
    last_committed_message_index: conversation.last_committed_message_index ?? conversation.last_message_index ?? null
  };
  const lastIndex = Number(normalized.last_committed_message_index);
  const count = Number(conversation.message_count);
  normalized.message_count = Number.isFinite(count)
    ? count
    : Number.isFinite(lastIndex)
      ? lastIndex + 1
      : 0;
  normalized.foreground_sessions = (Array.isArray(conversation.foreground_sessions)
    ? conversation.foreground_sessions
    : []
  ).map((session) => normalizeForegroundSessionSummary(session, normalized));
  if (!normalized.foreground_sessions.length) {
    normalized.foreground_sessions = [normalizeForegroundSessionSummary({
      id: 'main',
      foreground_session_id: normalized.foreground_session_id || 'main',
      nickname: normalized.nickname,
      message_count: normalized.message_count,
      last_message_id: normalized.last_message_id,
      last_message_time: normalized.last_message_time,
      last_committed_message_id: normalized.last_committed_message_id,
      last_committed_message_index: normalized.last_committed_message_index,
      last_seen_message_id: normalized.last_seen_message_id,
      last_seen_at: normalized.last_seen_at,
      is_main: true
    }, normalized)];
  }
  return normalized;
}

export function displayConversationName(conversation) {
  if (!conversation) return '';
  return (
    (conversation.nickname || '').trim()
    || conversation.platform_chat_id
    || conversation.conversation_id
  );
}

export function foregroundSessions(conversation) {
  if (!conversation) return [];
  const sessions = Array.isArray(conversation?.foreground_sessions)
    ? conversation.foreground_sessions
    : [];
  if (sessions.length > 0) return sessions.map((session) => normalizeForegroundSessionSummary(session, conversation));
  return [{
    id: 'main',
    session_id: conversation?.foreground_session_id || 'local__agent__foreground__main',
    nickname: displayConversationName(conversation),
    message_count: conversation?.message_count || 0,
    last_message_id: conversation?.last_message_id || null,
    last_message_time: conversation?.last_message_time || null,
    last_seen_message_id: conversation?.last_seen_message_id || null,
    is_main: true
  }];
}

export function displayForegroundSessionName(session, conversation) {
  return (
    String(session?.nickname || '').trim()
    || (session?.id === 'main' ? displayConversationName(conversation) : '')
    || session?.id
    || 'Main'
  );
}

export function selectedForegroundSessionId(selected) {
  return selected?.foregroundSessionId || selected?.sessionId || 'main';
}

export async function api(serverId, path, options = {}) {
  return window.stellacode2.request({
    serverId,
    path,
    method: options.method || 'GET',
    body: options.body
  });
}

export async function connectionInfo(serverId) {
  return window.stellacode2.connectionInfo(serverId);
}

export async function loadConversations(serverId) {
  const snapshot = await loadHomeSnapshot(serverId);
  return snapshot.conversations || [];
}

export async function markConversationSeen(serverId, conversationId, lastSeenMessageId, foregroundSessionId = 'main') {
  const response = await api(serverId, `/api/conversations/${conversationId}/seen`, {
    method: 'POST',
    body: {
      last_seen_message_id: String(lastSeenMessageId),
      foreground_session_id: foregroundSessionId || 'main'
    }
  });
  return response.data?.seen || null;
}

export async function conversationStreamUrl(serverId) {
  const info = await connectionInfo(serverId);
  const url = new URL('/api/ws/home', info.baseUrl);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('token', info.token || '');
  return url.toString();
}

export async function loadHomeSnapshot(serverId, timeoutMs = 5000) {
  const url = await conversationStreamUrl(serverId);
  return new Promise((resolve, reject) => {
    let settled = false;
    const socket = new WebSocket(url);
    const timeout = window.setTimeout(() => {
      if (settled) return;
      settled = true;
      try {
        socket.close();
      } catch {}
      reject(new Error('Home snapshot timeout'));
    }, timeoutMs);
    const finish = (result, error) => {
      if (settled) return;
      settled = true;
      window.clearTimeout(timeout);
      try {
        socket.close();
      } catch {}
      if (error) reject(error);
      else resolve(result);
    };
    socket.addEventListener('message', (event) => {
      let payload;
      try {
        payload = JSON.parse(event.data);
      } catch {
        return;
      }
      if (payload?.type !== 'home.snapshot') return;
      finish({
        ...payload,
        conversations: (payload.conversations || []).map(normalizeConversationSummary)
      });
    });
    socket.addEventListener('error', () => finish(null, new Error('Home snapshot websocket failed')));
    socket.addEventListener('close', () => {
      if (!settled) finish(null, new Error('Home snapshot websocket closed'));
    });
  });
}

export async function createConversation(serverId, options = {}) {
  const body = {};
  const nickname = String(options.nickname || '').trim();
  if (nickname) body.nickname = nickname;
  const response = await api(serverId, '/api/conversations', {
    method: 'POST',
    body
  });
  return response.data;
}

export async function renameConversation(serverId, conversationId, nickname) {
  const response = await api(serverId, `/api/conversations/${conversationId}`, {
    method: 'PATCH',
    body: { nickname }
  });
  return response.data?.conversation || null;
}

export async function deleteConversation(serverId, conversationId) {
  return api(serverId, `/api/conversations/${conversationId}`, {
    method: 'DELETE'
  });
}

export async function loadForegroundSessions(serverId, conversationId) {
  const snapshot = await loadHomeSnapshot(serverId);
  const conversation = snapshot.conversations.find((item) => item.conversation_id === conversationId);
  return foregroundSessions(conversation);
}

export async function createForegroundSession(serverId, conversationId, options = {}) {
  const body = {};
  const sessionId = String(options.sessionId || options.session_id || '').trim();
  const nickname = String(options.nickname || '').trim();
  if (sessionId) body.session_id = sessionId;
  if (nickname) body.nickname = nickname;
  const response = await api(serverId, `/api/conversations/${conversationId}/foreground_sessions`, {
    method: 'POST',
    body
  });
  return response.data?.foreground_session || null;
}

export async function renameForegroundSession(serverId, conversationId, foregroundSessionId, nickname) {
  const response = await api(serverId, `/api/conversations/${conversationId}/foreground_sessions/${encodeURIComponent(foregroundSessionId || 'main')}`, {
    method: 'PATCH',
    body: { nickname }
  });
  return response.data?.foreground_session || null;
}

export async function deleteForegroundSession(serverId, conversationId, foregroundSessionId) {
  return api(serverId, `/api/conversations/${conversationId}/foreground_sessions/${encodeURIComponent(foregroundSessionId || 'main')}`, {
    method: 'DELETE'
  });
}

export async function loadMessages(serverId, conversationId, options = {}) {
  const offset = Math.max(0, Number(options.offset || 0));
  const limit = Math.max(1, Math.min(200, Number(options.limit || 40)));
  const foregroundSessionId = options.foregroundSessionId || options.sessionId || 'main';
  const response = await api(
    serverId,
    `/api/conversations/${conversationId}/foreground_sessions/${encodeURIComponent(foregroundSessionId)}/messages?offset=${encodeURIComponent(offset)}&limit=${encodeURIComponent(limit)}`
  );
  return response.data?.messages || [];
}

export async function postConversationMessage(serverId, conversationId, text, userName = 'workspace-user', files = [], selectionReferences = [], foregroundSessionId = 'main', clientMessageId = '') {
  return api(serverId, `/api/conversations/${conversationId}/foreground_sessions/${encodeURIComponent(foregroundSessionId || 'main')}/messages`, {
    method: 'POST',
    body: {
      client_message_id: String(clientMessageId || '').trim() || undefined,
      user_name: String(userName || '').trim() || 'workspace-user',
      text,
      selection_references: Array.isArray(selectionReferences) && selectionReferences.length > 0 ? selectionReferences : undefined,
      files: Array.isArray(files) && files.length > 0 ? files : undefined
    }
  });
}

export async function loadStatus(serverId, conversationId) {
  const snapshot = await loadHomeSnapshot(serverId);
  const conversation = snapshot.conversations.find((item) => item.conversation_id === conversationId);
  return conversation || {};
}

export async function loadModels(serverId) {
  const response = await api(serverId, '/api/models');
  return response.data?.models || [];
}

export async function loadWorkspace(serverId, conversationId, path = '', limit = 300) {
  const response = await api(
    serverId,
    `/api/conversations/${conversationId}/workspace?path=${encodeURIComponent(path || '')}&limit=${encodeURIComponent(limit)}`
  );
  return response.data;
}

export async function loadWorkspaceFile(serverId, conversationId, path, limitBytes = 2_000_000) {
  const response = await api(
    serverId,
    `/api/conversations/${conversationId}/workspace/file?path=${encodeURIComponent(path || '')}&offset=0&limit_bytes=${encodeURIComponent(limitBytes)}`
  );
  return response.data;
}

export async function listTerminals(serverId, conversationId) {
  const response = await api(serverId, `/api/conversations/${conversationId}/terminals`);
  return response.data?.terminals || [];
}

export async function createTerminal(serverId, conversationId, options = {}) {
  const response = await api(serverId, `/api/conversations/${conversationId}/terminals`, {
    method: 'POST',
    body: options
  });
  return response.data;
}

export async function terminateTerminal(serverId, conversationId, terminalId) {
  const response = await api(serverId, `/api/conversations/${conversationId}/terminals/${terminalId}`, {
    method: 'DELETE'
  });
  return response.data;
}

export async function terminalStreamUrl(serverId, conversationId, terminalId, offset = 0) {
  const info = await connectionInfo(serverId);
  const url = new URL(
    `/api/conversations/${encodeURIComponent(conversationId)}/terminals/${encodeURIComponent(terminalId)}/ws`,
    info.baseUrl
  );
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('token', info.token || '');
  url.searchParams.set('offset', String(Math.max(0, Number(offset) || 0)));
  return url.toString();
}
