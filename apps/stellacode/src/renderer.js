const $ = (selector) => document.querySelector(selector);

const state = {
  settings: null,
  activeServerId: null,
  selected: null,
  conversations: new Map(),
  statuses: new Map(),
  serverHealth: new Map(),
  messages: [],
  messageDetails: new Map(),
  expandedMessages: new Set(),
  activePoll: null,
  saveTimer: null
};

const elements = {
  conversationList: $('#conversationList'),
  messageList: $('#messageList'),
  conversationTitle: $('#conversationTitle'),
  conversationSubtitle: $('#conversationSubtitle'),
  composerInput: $('#composerInput'),
  composerHint: $('#composerHint'),
  sendButton: $('#sendButton'),
  refreshButton: $('#refreshButton'),
  newConversationButton: $('#newConversationButton'),
  serverStatusButton: $('#serverStatusButton'),
  serverPopover: $('#serverPopover'),
  settingsButton: $('#settingsButton'),
  modalLayer: $('#modalLayer')
};

function conversationKey(serverId, conversationId) {
  return `${serverId}:${conversationId}`;
}

function safeText(value) {
  return String(value ?? '');
}

function escapeHtml(value) {
  return safeText(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#039;');
}

function formatRelative(value) {
  if (!value) {
    return '';
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return '';
  }
  const seconds = Math.max(1, Math.round((Date.now() - date.getTime()) / 1000));
  if (seconds < 60) {
    return '刚刚';
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} 分钟`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 24) {
    return `${hours} 小时`;
  }
  const days = Math.round(hours / 24);
  if (days < 30) {
    return `${days} 天`;
  }
  return `${Math.round(days / 30)} 个月`;
}

function createId(prefix) {
  if (crypto.randomUUID) {
    return `${prefix}-${crypto.randomUUID()}`;
  }
  return `${prefix}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function getServers() {
  return state.settings?.servers || [];
}

function selectedStatus() {
  if (!state.selected) {
    return null;
  }
  return state.statuses.get(conversationKey(state.selected.serverId, state.selected.conversationId));
}

function displayConversationName(serverId, conversation) {
  const key = conversationKey(serverId, conversation.conversation_id);
  return state.settings.conversationNames[key] || conversation.platform_chat_id || conversation.conversation_id;
}

function isRemoteStatus(status) {
  if (!status?.remote) {
    return false;
  }
  const normalized = String(status.remote).toLowerCase();
  return !['selectable', 'disabled', 'local', 'none'].includes(normalized);
}

function api(serverId, path, options = {}) {
  return window.stellacode.request({
    serverId,
    path,
    method: options.method || 'GET',
    body: options.body
  });
}

function saveSettingsSoon() {
  clearTimeout(state.saveTimer);
  state.saveTimer = setTimeout(async () => {
    state.settings = await window.stellacode.saveSettings(state.settings);
  }, 200);
}

function setHealth(serverId, patch) {
  const current = state.serverHealth.get(serverId) || {};
  state.serverHealth.set(serverId, {
    ...current,
    ...patch,
    checkedAt: new Date().toISOString()
  });
}

async function mapLimit(items, limit, worker) {
  const results = [];
  let index = 0;
  const runners = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (index < items.length) {
      const current = index;
      index += 1;
      results[current] = await worker(items[current], current);
    }
  });
  await Promise.all(runners);
  return results;
}

async function refreshServer(serverId) {
  const server = getServers().find((item) => item.id === serverId);
  if (!server) {
    return;
  }
  try {
    const response = await api(serverId, '/api/conversations?offset=0&limit=100');
    const conversations = response.data?.conversations || [];
    state.conversations.set(serverId, conversations);
    setHealth(serverId, {
      state: 'online',
      total: response.data?.total ?? conversations.length,
      error: ''
    });
    await mapLimit(conversations.slice(0, 60), 4, async (conversation) => {
      try {
        const status = await api(serverId, `/api/conversations/${conversation.conversation_id}/status`);
        state.statuses.set(conversationKey(serverId, conversation.conversation_id), status.data);
      } catch {
        state.statuses.delete(conversationKey(serverId, conversation.conversation_id));
      }
    });
  } catch (error) {
    state.conversations.set(serverId, []);
    setHealth(serverId, {
      state: 'offline',
      total: 0,
      error: error.message
    });
  }
  renderSidebar();
  renderServerPopover();
}

async function refreshAllServers() {
  await Promise.all(getServers().map((server) => refreshServer(server.id)));
}

async function selectConversation(serverId, conversationId) {
  state.selected = { serverId, conversationId };
  state.activeServerId = serverId;
  state.settings.activeServerId = serverId;
  state.messages = [];
  state.messageDetails.clear();
  state.expandedMessages.clear();
  saveSettingsSoon();
  renderSidebar();
  renderHeader();
  renderMessages();
  await refreshConversation();
}

async function refreshConversation() {
  if (!state.selected) {
    renderHeader();
    renderMessages();
    return;
  }
  const { serverId, conversationId } = state.selected;
  try {
    const messages = await api(serverId, `/api/conversations/${conversationId}/messages?offset=0&limit=200`);
    state.messages = messages.data?.messages || [];
    const status = await api(serverId, `/api/conversations/${conversationId}/status`);
    state.statuses.set(conversationKey(serverId, conversationId), status.data);
  } catch (error) {
    state.messages = [];
    setHealth(serverId, { state: 'offline', error: error.message, total: 0 });
  }
  renderSidebar();
  renderHeader();
  renderMessages();
}

async function fetchMessageDetail(messageId) {
  if (!state.selected || state.messageDetails.has(messageId)) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  const response = await api(serverId, `/api/conversations/${conversationId}/messages/${messageId}`);
  state.messageDetails.set(messageId, response.data);
}

async function toggleMessage(messageId) {
  if (state.expandedMessages.has(messageId)) {
    state.expandedMessages.delete(messageId);
  } else {
    state.expandedMessages.add(messageId);
    await fetchMessageDetail(messageId);
  }
  renderMessages();
}

async function createConversation(serverId, localName) {
  const response = await api(serverId, '/api/conversations', {
    method: 'POST',
    body: {
      platform_chat_id: createId('stellacode')
    }
  });
  const conversationId = response.data.conversation_id;
  if (localName.trim()) {
    state.settings.conversationNames[conversationKey(serverId, conversationId)] = localName.trim();
    saveSettingsSoon();
  }
  closeModal();
  await refreshServer(serverId);
  await selectConversation(serverId, conversationId);
}

async function sendMessage() {
  if (!state.selected) {
    return;
  }
  const text = elements.composerInput.value.trim();
  if (!text) {
    return;
  }
  elements.composerInput.value = '';
  autosizeComposer();
  elements.sendButton.disabled = true;
  const { serverId, conversationId } = state.selected;
  try {
    await api(serverId, `/api/conversations/${conversationId}/messages`, {
      method: 'POST',
      body: {
        user_name: 'Stellacode',
        text
      }
    });
    await refreshConversation();
    pollActiveConversation();
  } catch (error) {
    showToast(error.message);
  } finally {
    elements.sendButton.disabled = false;
  }
}

function pollActiveConversation() {
  clearInterval(state.activePoll);
  let remaining = 18;
  state.activePoll = setInterval(async () => {
    if (!state.selected || remaining <= 0) {
      clearInterval(state.activePoll);
      state.activePoll = null;
      return;
    }
    remaining -= 1;
    await refreshConversation();
  }, 2500);
}

function renameConversation(serverId, conversation) {
  const current = displayConversationName(serverId, conversation);
  const name = window.prompt('会话名称', current);
  if (name === null) {
    return;
  }
  const key = conversationKey(serverId, conversation.conversation_id);
  if (name.trim()) {
    state.settings.conversationNames[key] = name.trim();
  } else {
    delete state.settings.conversationNames[key];
  }
  saveSettingsSoon();
  renderSidebar();
  renderHeader();
}

function renderSidebar() {
  const fragment = document.createDocumentFragment();
  for (const server of getServers()) {
    const conversations = state.conversations.get(server.id) || [];
    const group = document.createElement('div');
    group.className = 'conversation-group';
    group.innerHTML = `
      <div class="conversation-group-title">
        <span>${escapeHtml(server.name)}</span>
        <button class="tiny-button" data-refresh-server="${escapeHtml(server.id)}" title="刷新">↻</button>
      </div>
    `;
    for (const conversation of conversations) {
      const status = state.statuses.get(conversationKey(server.id, conversation.conversation_id));
      const selected =
        state.selected?.serverId === server.id &&
        state.selected?.conversationId === conversation.conversation_id;
      const row = document.createElement('button');
      row.type = 'button';
      row.className = `conversation-row${selected ? ' selected' : ''}${isRemoteStatus(status) ? ' remote' : ''}`;
      row.dataset.serverId = server.id;
      row.dataset.conversationId = conversation.conversation_id;
      const remoteMeta = isRemoteStatus(status)
        ? `<span class="remote-meta">${escapeHtml(status.remote)} · ${escapeHtml(status.workspace)}</span>`
        : '';
      row.innerHTML = `
        <span class="conversation-main">
          <span class="conversation-name">${escapeHtml(displayConversationName(server.id, conversation))}</span>
          ${remoteMeta}
        </span>
        <span class="conversation-age">${escapeHtml(conversation.model || '')}</span>
      `;
      row.addEventListener('click', () => selectConversation(server.id, conversation.conversation_id));
      row.addEventListener('dblclick', () => renameConversation(server.id, conversation));
      group.append(row);
    }
    if (conversations.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'empty-server';
      empty.textContent = '暂无会话';
      group.append(empty);
    }
    fragment.append(group);
  }
  elements.conversationList.replaceChildren(fragment);
  elements.conversationList.querySelectorAll('[data-refresh-server]').forEach((button) => {
    button.addEventListener('click', (event) => {
      event.stopPropagation();
      refreshServer(button.dataset.refreshServer);
    });
  });
}

function renderHeader() {
  if (!state.selected) {
    elements.conversationTitle.textContent = 'Stellacode';
    elements.conversationSubtitle.textContent = '选择或创建一个 Conversation';
    elements.composerHint.textContent = '未连接会话';
    return;
  }
  const server = getServers().find((item) => item.id === state.selected.serverId);
  const conversation = (state.conversations.get(state.selected.serverId) || []).find(
    (item) => item.conversation_id === state.selected.conversationId
  );
  const status = selectedStatus();
  elements.conversationTitle.textContent = conversation
    ? displayConversationName(state.selected.serverId, conversation)
    : state.selected.conversationId;
  elements.conversationSubtitle.textContent = `${server?.name || state.selected.serverId} · ${
    status?.model || conversation?.model || 'model pending'
  }`;
  elements.composerHint.textContent = isRemoteStatus(status)
    ? `Remote: ${status.remote} · ${status.workspace}`
    : '本地模式';
}

function roleClass(role) {
  return String(role || '').toLowerCase() === 'user' ? 'user' : 'assistant';
}

function renderMessages() {
  if (!state.selected) {
    elements.messageList.innerHTML = `
      <div class="empty-state">
        <div class="empty-title">连接到 Stellaclaw</div>
        <div class="empty-copy">先在设置里添加服务器，然后创建或选择一个 Conversation。</div>
      </div>
    `;
    return;
  }
  if (state.messages.length === 0) {
    elements.messageList.innerHTML = `
      <div class="empty-state">
        <div class="empty-title">新的 Conversation</div>
        <div class="empty-copy">可以直接开始输入，也可以先发送 /model 选择模型。</div>
      </div>
    `;
    return;
  }
  const fragment = document.createDocumentFragment();
  for (const message of state.messages) {
    const expanded = state.expandedMessages.has(message.id);
    const detail = state.messageDetails.get(message.id);
    const article = document.createElement('article');
    article.className = `message ${roleClass(message.role)}${expanded ? ' expanded' : ''}`;
    article.dataset.messageId = message.id;
    const bodyText = expanded ? detail?.rendered_text || message.preview : message.preview;
    article.innerHTML = `
      <button class="message-bubble" type="button">
        <span class="message-meta">${escapeHtml(message.user_name || message.role || 'assistant')} ${
          message.message_time ? `· ${escapeHtml(formatRelative(message.message_time))}` : ''
        }</span>
        <span class="message-text">${escapeHtml(bodyText)}</span>
        <span class="message-foot">${expanded ? '收起详情' : '展开详情'}${
          message.has_token_usage ? ' · usage' : ''
        }</span>
      </button>
    `;
    article.querySelector('.message-bubble').addEventListener('click', () => toggleMessage(message.id));
    fragment.append(article);
  }
  elements.messageList.replaceChildren(fragment);
  elements.messageList.scrollTop = elements.messageList.scrollHeight;
}

function renderServerPopover() {
  if (elements.serverPopover.classList.contains('hidden')) {
    return;
  }
  const rows = getServers()
    .map((server) => {
      const health = state.serverHealth.get(server.id) || { state: 'unknown' };
      return `
        <button class="server-row" type="button" data-server-row="${escapeHtml(server.id)}">
          <span>
            <strong>${escapeHtml(server.name)}</strong>
            <small>${escapeHtml(server.connectionMode === 'ssh_proxy' ? server.sshHost : server.baseUrl)}</small>
          </span>
          <span class="server-state ${escapeHtml(health.state || 'unknown')}">${escapeHtml(
            health.state || 'unknown'
          )}</span>
        </button>
      `;
    })
    .join('');
  elements.serverPopover.innerHTML = `
    <div class="popover-head">
      <span>Server Status</span>
      <button id="refreshAllServersButton" class="tiny-button" type="button">刷新全部</button>
    </div>
    <div class="server-list">${rows || '<div class="empty-server">没有服务器</div>'}</div>
  `;
  $('#refreshAllServersButton')?.addEventListener('click', refreshAllServers);
  elements.serverPopover.querySelectorAll('[data-server-row]').forEach((row) => {
    row.addEventListener('click', async () => {
      state.activeServerId = row.dataset.serverRow;
      await refreshServer(row.dataset.serverRow);
    });
  });
}

function openNewConversationModal() {
  const servers = getServers()
    .map(
      (server) => `
      <button class="choice-row" type="button" data-create-server="${escapeHtml(server.id)}">
        <span>
          <strong>${escapeHtml(server.name)}</strong>
          <small>${escapeHtml(server.connectionMode === 'ssh_proxy' ? `SSH · ${server.sshHost}` : server.baseUrl)}</small>
        </span>
      </button>
    `
    )
    .join('');
  openModal(`
    <div class="modal-card small">
      <div class="modal-head">
        <h2>新建 Conversation</h2>
        <button class="icon-button" type="button" data-close-modal>×</button>
      </div>
      <label class="field-label modal-field">
        本地名称
        <input id="newConversationName" type="text" placeholder="可选" />
      </label>
      <div class="choice-list">${servers || '<div class="empty-state compact">先在设置里添加服务器</div>'}</div>
    </div>
  `);
  elements.modalLayer.querySelectorAll('[data-create-server]').forEach((button) => {
    button.addEventListener('click', () => {
      const name = $('#newConversationName').value;
      createConversation(button.dataset.createServer, name).catch((error) => showToast(error.message));
    });
  });
}

function openSettingsModal() {
  const servers = getServers();
  const rows = servers
    .map(
      (server) => `
      <div class="server-editor" data-editor-id="${escapeHtml(server.id)}">
        <div class="server-editor-title">
          <input data-field="name" value="${escapeHtml(server.name)}" />
          <button class="tiny-button danger" type="button" data-remove-server="${escapeHtml(server.id)}">删除</button>
        </div>
        <label class="field-label">连接方式
          <select data-field="connectionMode">
            <option value="direct" ${server.connectionMode === 'direct' ? 'selected' : ''}>Direct URL</option>
            <option value="ssh_proxy" ${server.connectionMode === 'ssh_proxy' ? 'selected' : ''}>SSH Proxy</option>
          </select>
        </label>
        <label class="field-label">服务器地址
          <input data-field="baseUrl" value="${escapeHtml(server.baseUrl)}" placeholder="http://127.0.0.1:3111" />
        </label>
        <label class="field-label">Authorization Token
          <input data-field="token" value="${escapeHtml(server.token)}" type="password" />
        </label>
        <div class="field-grid">
          <label class="field-label">SSH Host
            <input data-field="sshHost" value="${escapeHtml(server.sshHost)}" placeholder="user@host" />
          </label>
          <label class="field-label">目标地址
            <input data-field="targetUrl" value="${escapeHtml(server.targetUrl)}" placeholder="http://127.0.0.1:3111" />
          </label>
        </div>
      </div>
    `
    )
    .join('');
  openModal(`
    <div class="modal-card settings-card">
      <div class="modal-head">
        <h2>服务器设置</h2>
        <button class="icon-button" type="button" data-close-modal>×</button>
      </div>
      <div class="settings-list">${rows}</div>
      <div class="modal-actions">
        <button id="addServerButton" class="secondary-button" type="button">添加服务器</button>
        <button id="saveSettingsButton" class="primary-button" type="button">保存</button>
      </div>
    </div>
  `);
  $('#addServerButton').addEventListener('click', () => {
    state.settings.servers.push({
      id: createId('server'),
      name: 'New Server',
      connectionMode: 'direct',
      baseUrl: 'http://127.0.0.1:3111',
      targetUrl: 'http://127.0.0.1:3111',
      sshHost: '',
      token: ''
    });
    openSettingsModal();
  });
  elements.modalLayer.querySelectorAll('[data-remove-server]').forEach((button) => {
    button.addEventListener('click', () => {
      state.settings.servers = state.settings.servers.filter((server) => server.id !== button.dataset.removeServer);
      openSettingsModal();
    });
  });
  $('#saveSettingsButton').addEventListener('click', saveSettingsFromModal);
}

async function saveSettingsFromModal() {
  const nextServers = [];
  elements.modalLayer.querySelectorAll('[data-editor-id]').forEach((editor) => {
    const read = (field) => editor.querySelector(`[data-field="${field}"]`)?.value || '';
    nextServers.push({
      id: editor.dataset.editorId,
      name: read('name').trim() || 'Server',
      connectionMode: read('connectionMode') === 'ssh_proxy' ? 'ssh_proxy' : 'direct',
      baseUrl: read('baseUrl').trim() || 'http://127.0.0.1:3111',
      targetUrl: read('targetUrl').trim() || read('baseUrl').trim() || 'http://127.0.0.1:3111',
      sshHost: read('sshHost').trim(),
      token: read('token')
    });
  });
  state.settings.servers = nextServers;
  state.settings.activeServerId = nextServers[0]?.id || null;
  state.activeServerId = state.settings.activeServerId;
  state.settings = await window.stellacode.saveSettings(state.settings);
  closeModal();
  await refreshAllServers();
}

function openModal(html) {
  elements.modalLayer.innerHTML = html;
  elements.modalLayer.classList.remove('hidden');
  elements.modalLayer.querySelectorAll('[data-close-modal]').forEach((button) => {
    button.addEventListener('click', closeModal);
  });
}

function closeModal() {
  elements.modalLayer.classList.add('hidden');
  elements.modalLayer.innerHTML = '';
}

function showToast(message) {
  const toast = document.createElement('div');
  toast.className = 'toast';
  toast.textContent = message;
  document.body.append(toast);
  setTimeout(() => toast.remove(), 3600);
}

function autosizeComposer() {
  elements.composerInput.style.height = 'auto';
  elements.composerInput.style.height = `${Math.min(elements.composerInput.scrollHeight, 180)}px`;
}

async function init() {
  state.settings = await window.stellacode.loadSettings();
  state.activeServerId = state.settings.activeServerId;
  bindEvents();
  renderSidebar();
  renderHeader();
  renderMessages();
  await refreshAllServers();
}

function bindEvents() {
  elements.newConversationButton.addEventListener('click', openNewConversationModal);
  elements.settingsButton.addEventListener('click', openSettingsModal);
  elements.refreshButton.addEventListener('click', refreshConversation);
  elements.sendButton.addEventListener('click', sendMessage);
  elements.serverStatusButton.addEventListener('click', () => {
    elements.serverPopover.classList.toggle('hidden');
    renderServerPopover();
  });
  elements.composerInput.addEventListener('input', autosizeComposer);
  elements.composerInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      sendMessage();
    }
  });
  elements.modalLayer.addEventListener('click', (event) => {
    if (event.target === elements.modalLayer) {
      closeModal();
    }
  });
}

init().catch((error) => showToast(error.message));
