export function conversationKey(serverId, conversationId) {
  return `${serverId}:${conversationId}`;
}

export function displayConversationName(conversation) {
  return (
    (conversation.nickname || '').trim()
    || conversation.platform_chat_id
    || conversation.conversation_id
  );
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

export async function markConversationSeen(serverId, conversationId, lastSeenMessageId) {
  const response = await api(serverId, `/api/conversations/${conversationId}/seen`, {
    method: 'POST',
    body: { last_seen_message_id: String(lastSeenMessageId) }
  });
  return response.data?.seen || null;
}

export async function conversationStreamUrl(serverId) {
  const info = await connectionInfo(serverId);
  const url = new URL('/api/conversations/stream', info.baseUrl);
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

export async function loadMessages(serverId, conversationId, options = {}) {
  const offset = Math.max(0, Number(options.offset || 0));
  const limit = Math.max(1, Math.min(200, Number(options.limit || 40)));
  const response = await api(
    serverId,
    `/api/conversations/${conversationId}/messages?offset=${encodeURIComponent(offset)}&limit=${encodeURIComponent(limit)}`
  );
  return response.data?.messages || [];
}

export async function postConversationMessage(serverId, conversationId, text, userName = 'workspace-user') {
  return api(serverId, `/api/conversations/${conversationId}/messages`, {
    method: 'POST',
    body: {
      user_name: String(userName || '').trim() || 'workspace-user',
      text
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
