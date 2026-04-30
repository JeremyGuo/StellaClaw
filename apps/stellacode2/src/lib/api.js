export function conversationKey(serverId, conversationId) {
  return `${serverId}:${conversationId}`;
}

export function displayConversationName(settings, serverId, conversation) {
  const key = conversationKey(serverId, conversation.conversation_id);
  return (
    (conversation.nickname || '').trim()
    || settings?.conversationNames?.[key]
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

export async function loadMessages(serverId, conversationId) {
  const response = await api(serverId, `/api/conversations/${conversationId}/messages?limit=40&order=asc`);
  return response.data?.messages || [];
}

export async function loadMessagesAfter(serverId, conversationId, messageId, limit = 80) {
  const response = await api(
    serverId,
    `/api/conversations/${conversationId}/messages/after/${encodeURIComponent(messageId)}?limit=${encodeURIComponent(limit)}`
  );
  return response.data?.messages || [];
}

export async function loadMessageRange(serverId, conversationId, anchorId, options = {}) {
  const direction = options.direction || 'after';
  const includeAnchor = options.includeAnchor ? 'true' : 'false';
  const limit = options.limit || 120;
  const response = await api(
    serverId,
    `/api/conversations/${conversationId}/messages/range?anchor_id=${encodeURIComponent(anchorId)}&direction=${encodeURIComponent(direction)}&include_anchor=${includeAnchor}&limit=${encodeURIComponent(limit)}`
  );
  return response.data?.messages || [];
}

export async function postConversationMessage(serverId, conversationId, text) {
  return api(serverId, `/api/conversations/${conversationId}/messages`, {
    method: 'POST',
    body: {
      user_name: 'Stellacode',
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
