export function conversationKey(serverId, conversationId, foregroundSessionId = 'main') {
  return `${serverId}:${conversationId}:${foregroundSessionId || 'main'}`;
}

export function displayConversationName(conversation) {
  return (
    (conversation.nickname || '').trim()
    || conversation.platform_chat_id
    || conversation.conversation_id
  );
}

export function foregroundSessions(conversation) {
  const sessions = Array.isArray(conversation?.foreground_sessions)
    ? conversation.foreground_sessions
    : [];
  if (sessions.length > 0) return sessions;
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
  const response = await api(serverId, '/api/conversations?limit=80');
  return response.data?.conversations || [];
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
  const response = await api(serverId, `/api/conversations/${conversationId}/foreground_sessions`);
  return response.data?.foreground_sessions || [];
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

export async function postConversationMessage(serverId, conversationId, text, userName = 'workspace-user', files = [], selectionReferences = [], foregroundSessionId = 'main') {
  return api(serverId, `/api/conversations/${conversationId}/foreground_sessions/${encodeURIComponent(foregroundSessionId || 'main')}/messages`, {
    method: 'POST',
    body: {
      user_name: String(userName || '').trim() || 'workspace-user',
      text,
      selection_references: Array.isArray(selectionReferences) && selectionReferences.length > 0 ? selectionReferences : undefined,
      files: Array.isArray(files) && files.length > 0 ? files : undefined
    }
  });
}

export async function loadStatus(serverId, conversationId) {
  const response = await api(serverId, `/api/conversations/${conversationId}/status`);
  return response.data;
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
    `/api/conversations/${encodeURIComponent(conversationId)}/terminals/${encodeURIComponent(terminalId)}/stream`,
    info.baseUrl
  );
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('token', info.token || '');
  url.searchParams.set('offset', String(Math.max(0, Number(offset) || 0)));
  return url.toString();
}
