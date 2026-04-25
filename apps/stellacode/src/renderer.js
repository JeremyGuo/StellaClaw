const $ = (selector) => document.querySelector(selector);

const state = {
  settings: null,
  activeServerId: null,
  selected: null,
  conversations: new Map(),
  statuses: new Map(),
  serverHealth: new Map(),
  messages: [],
  messagesSignature: '',
  messageDetails: new Map(),
  expandedMessages: new Set(),
  activeContextTab: 'overview',
  activePreviewMessageId: null,
  contextCollapsed: true,
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
  composerModePill: $('#composerModePill'),
  sendButton: $('#sendButton'),
  attachButton: $('#attachButton'),
  refreshButton: $('#refreshButton'),
  toggleContextButton: $('#toggleContextButton'),
  newConversationButton: $('#newConversationButton'),
  serverStatusButton: $('#serverStatusButton'),
  serverPopover: $('#serverPopover'),
  settingsButton: $('#settingsButton'),
  contextContent: $('#contextContent'),
  collapseContextButton: $('#collapseContextButton'),
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

function formatCompactNumber(value) {
  const number = Number(value || 0);
  if (number >= 1_000_000) {
    return `${(number / 1_000_000).toFixed(1)}M`;
  }
  if (number >= 10_000) {
    return `${Math.round(number / 1000)}K`;
  }
  return number.toLocaleString();
}

function formatCost(value) {
  return `$${Number(value || 0).toFixed(3)}`;
}

function stableSignature(value) {
  return JSON.stringify(value ?? null);
}

function messageListSignature(messages) {
  return stableSignature(
    messages.map((message) => ({
      id: message.id,
      role: message.role,
      user_name: message.user_name,
      message_time: message.message_time,
      preview: message.preview,
      has_token_usage: message.has_token_usage
    }))
  );
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

function statusUsageTotals(status) {
  const totals = {
    cacheRead: 0,
    cacheWrite: 0,
    input: 0,
    output: 0,
    cost: 0
  };
  for (const bucket of Object.values(status?.usage || {})) {
    totals.cacheRead += Number(bucket?.cache_read || 0);
    totals.cacheWrite += Number(bucket?.cache_write || 0);
    totals.input += Number(bucket?.uncache_input || 0);
    totals.output += Number(bucket?.output || 0);
    const cost = bucket?.cost || {};
    totals.cost +=
      Number(cost.cache_read || 0) +
      Number(cost.cache_write || 0) +
      Number(cost.uncache_input || 0) +
      Number(cost.output || 0);
  }
  totals.totalTokens = totals.cacheRead + totals.cacheWrite + totals.input + totals.output;
  totals.cacheHit = totals.cacheRead + totals.input > 0 ? totals.cacheRead / (totals.cacheRead + totals.input) : 0;
  return totals;
}

function safeLinkHref(value) {
  const text = safeText(value).trim();
  if (/^(https?:\/\/|mailto:)/i.test(text)) {
    return text;
  }
  return '';
}

function renderInlineMarkdown(value) {
  const codeSpans = [];
  const protectedText = safeText(value).replace(/`([^`\n]+)`/g, (_match, code) => {
    const token = `%%STELLACODE_CODE_${codeSpans.length}%%`;
    codeSpans.push(`<code>${escapeHtml(code)}</code>`);
    return token;
  });
  let rendered = escapeHtml(protectedText);
  rendered = rendered.replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+|mailto:[^\s)]+)\)/gi, (_match, label, href) => {
    const safeHref = safeLinkHref(href);
    if (!safeHref) {
      return label;
    }
    return `<a href="${escapeHtml(safeHref)}" target="_blank" rel="noreferrer">${label}</a>`;
  });
  rendered = rendered
    .replace(/\*\*([^*\n][\s\S]*?[^*\n])\*\*/g, '<strong>$1</strong>')
    .replace(/__([^_\n][\s\S]*?[^_\n])__/g, '<strong>$1</strong>')
    .replace(/(^|[^*])\*([^*\n]+)\*/g, '$1<em>$2</em>')
    .replace(/(^|[^_])_([^_\n]+)_/g, '$1<em>$2</em>')
    .replace(/~~([^~\n]+)~~/g, '<del>$1</del>');
  codeSpans.forEach((html, index) => {
    rendered = rendered.replaceAll(`%%STELLACODE_CODE_${index}%%`, html);
  });
  return rendered;
}

function renderMarkdownLines(text) {
  const lines = text.replace(/\r\n/g, '\n').split('\n');
  const html = [];
  let paragraph = [];
  let listItems = [];
  let listOrdered = false;
  let quote = [];

  const flushParagraph = () => {
    if (paragraph.length === 0) {
      return;
    }
    html.push(`<p>${paragraph.map(renderInlineMarkdown).join('<br>')}</p>`);
    paragraph = [];
  };
  const flushList = () => {
    if (listItems.length === 0) {
      return;
    }
    const tag = listOrdered ? 'ol' : 'ul';
    html.push(`<${tag}>${listItems.map((item) => `<li>${renderInlineMarkdown(item)}</li>`).join('')}</${tag}>`);
    listItems = [];
  };
  const flushQuote = () => {
    if (quote.length === 0) {
      return;
    }
    html.push(`<blockquote>${renderMarkdownLines(quote.join('\n'))}</blockquote>`);
    quote = [];
  };
  const flushAll = () => {
    flushParagraph();
    flushList();
    flushQuote();
  };

  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) {
      flushAll();
      continue;
    }
    const heading = /^(#{1,6})\s+(.+)$/.exec(trimmed);
    if (heading) {
      flushAll();
      const level = Math.min(heading[1].length + 1, 6);
      html.push(`<h${level}>${renderInlineMarkdown(heading[2])}</h${level}>`);
      continue;
    }
    if (/^[-*_]{3,}$/.test(trimmed)) {
      flushAll();
      html.push('<hr>');
      continue;
    }
    const quoteMatch = /^>\s?(.*)$/.exec(line);
    if (quoteMatch) {
      flushParagraph();
      flushList();
      quote.push(quoteMatch[1]);
      continue;
    }
    const unordered = /^\s*[-*+]\s+(.+)$/.exec(line);
    const ordered = /^\s*\d+[.)]\s+(.+)$/.exec(line);
    if (unordered || ordered) {
      flushParagraph();
      flushQuote();
      const orderedLine = Boolean(ordered);
      if (listItems.length > 0 && listOrdered !== orderedLine) {
        flushList();
      }
      listOrdered = orderedLine;
      listItems.push((unordered || ordered)[1]);
      continue;
    }
    flushList();
    flushQuote();
    paragraph.push(line);
  }
  flushAll();
  return html.join('');
}

function renderMarkdownMessage(value) {
  const text = safeText(value).trim();
  if (!text) {
    return '<span class="message-empty">空消息</span>';
  }
  const blocks = text.split(/(```[\s\S]*?```)/g);
  return blocks
    .map((block) => {
      if (block.startsWith('```') && block.endsWith('```')) {
        const inner = block.slice(3, -3).replace(/^[\w-]+\n/, '');
        return `<pre class="code-card"><code>${escapeHtml(inner.replace(/^\n|\n$/g, ''))}</code></pre>`;
      }
      return renderMarkdownLines(block);
    })
    .join('');
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
  renderContext();
}

async function refreshAllServers() {
  await Promise.all(getServers().map((server) => refreshServer(server.id)));
}

async function selectConversation(serverId, conversationId) {
  state.selected = { serverId, conversationId };
  state.activeServerId = serverId;
  state.settings.activeServerId = serverId;
  state.messages = [];
  state.messagesSignature = '';
  state.messageDetails.clear();
  state.expandedMessages.clear();
  state.activePreviewMessageId = null;
  saveSettingsSoon();
  renderSidebar();
  renderHeader();
  renderMessages();
  renderContext();
  await refreshConversation();
}

async function refreshConversation() {
  if (!state.selected) {
    renderHeader();
    renderMessages();
    return;
  }
  const { serverId, conversationId } = state.selected;
  let shouldRenderMessages = false;
  try {
    const messages = await api(serverId, `/api/conversations/${conversationId}/messages?offset=0&limit=200`);
    const nextMessages = messages.data?.messages || [];
    const nextSignature = messageListSignature(nextMessages);
    if (nextSignature !== state.messagesSignature) {
      state.messages = nextMessages;
      state.messagesSignature = nextSignature;
      shouldRenderMessages = true;
    }
    const status = await api(serverId, `/api/conversations/${conversationId}/status`);
    state.statuses.set(conversationKey(serverId, conversationId), status.data);
  } catch (error) {
    if (state.messages.length > 0 || state.messagesSignature !== messageListSignature([])) {
      state.messages = [];
      state.messagesSignature = messageListSignature([]);
      shouldRenderMessages = true;
    }
    setHealth(serverId, { state: 'offline', error: error.message, total: 0 });
  }
  renderSidebar();
  renderHeader();
  if (shouldRenderMessages) {
    renderMessages();
  }
  renderContext();
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
  state.activePreviewMessageId = messageId;
  state.activeContextTab = 'detail';
  if (state.expandedMessages.has(messageId)) {
    state.expandedMessages.delete(messageId);
  } else {
    state.expandedMessages.add(messageId);
    await fetchMessageDetail(messageId);
  }
  renderMessages();
  renderContext();
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
  const rows = [];
  for (const server of getServers()) {
    const conversations = state.conversations.get(server.id) || [];
    for (const conversation of conversations) {
      rows.push({ server, conversation });
    }
  }

  rows.sort((left, right) =>
    left.conversation.conversation_id.localeCompare(right.conversation.conversation_id, undefined, {
      numeric: true
    })
  );

  for (const { server, conversation } of rows) {
    const status = state.statuses.get(conversationKey(server.id, conversation.conversation_id));
    const selected =
      state.selected?.serverId === server.id && state.selected?.conversationId === conversation.conversation_id;
    const row = document.createElement('button');
    row.type = 'button';
    row.className = `conversation-row${selected ? ' selected' : ''}${isRemoteStatus(status) ? ' remote' : ''}`;
    row.dataset.serverId = server.id;
    row.dataset.conversationId = conversation.conversation_id;
    const remoteMeta = isRemoteStatus(status)
      ? `<span class="remote-meta">${escapeHtml(status.remote)} · ${escapeHtml(compactPath(status.workspace))}</span>`
      : `<span class="remote-meta">${escapeHtml(server.name)}</span>`;
    row.innerHTML = `
      <span class="conversation-main">
        <span class="conversation-name">${escapeHtml(displayConversationName(server.id, conversation))}</span>
        ${remoteMeta}
      </span>
      <span class="conversation-age">${escapeHtml(conversation.model || '')}</span>
    `;
    row.addEventListener('click', () => selectConversation(server.id, conversation.conversation_id));
    row.addEventListener('dblclick', () => renameConversation(server.id, conversation));
    fragment.append(row);
  }

  if (rows.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty-server sidebar-empty';
    empty.textContent = '还没有 Conversation';
    fragment.append(empty);
  }

  elements.conversationList.replaceChildren(fragment);
}

function renderHeader() {
  if (!state.selected) {
    elements.conversationTitle.textContent = 'Stellacode';
    elements.conversationSubtitle.textContent = '选择或创建一个 Conversation';
    elements.composerHint.textContent = '未连接会话';
    elements.composerModePill.textContent = '未连接';
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
  elements.composerModePill.textContent = isRemoteStatus(status) ? 'Remote' : '本地';
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
    const activePreview = state.activePreviewMessageId === message.id;
    article.className = `message ${roleClass(message.role)}${expanded ? ' expanded' : ''}${
      activePreview ? ' active-preview' : ''
    }`;
    article.dataset.messageId = message.id;
    const bodyText = expanded ? detail?.rendered_text || message.preview : message.preview;
    article.innerHTML = `
      <div class="message-bubble" role="button" tabindex="0">
        <span class="message-meta">${escapeHtml(message.user_name || message.role || 'assistant')} ${
          message.message_time ? `· ${escapeHtml(formatRelative(message.message_time))}` : ''
        }</span>
        <div class="message-text">${renderMarkdownMessage(bodyText)}</div>
        <span class="message-actions">
          <span>${expanded ? '收起详情' : '展开详情'}</span>
          ${message.has_token_usage ? '<span>usage</span>' : ''}
        </span>
      </div>
    `;
    const bubble = article.querySelector('.message-bubble');
    bubble.addEventListener('click', () => toggleMessage(message.id));
    bubble.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        toggleMessage(message.id);
      }
    });
    fragment.append(article);
  }
  elements.messageList.replaceChildren(fragment);
  elements.messageList.scrollTop = elements.messageList.scrollHeight;
}

function compactPath(value) {
  const text = safeText(value);
  if (text.length <= 38) {
    return text;
  }
  const parts = text.split('/').filter(Boolean);
  if (parts.length >= 2) {
    return `…/${parts.slice(-2).join('/')}`;
  }
  return `…${text.slice(-35)}`;
}

function currentConversation() {
  if (!state.selected) {
    return null;
  }
  return (state.conversations.get(state.selected.serverId) || []).find(
    (item) => item.conversation_id === state.selected.conversationId
  );
}

function renderContext() {
  document.body.classList.toggle('context-collapsed', state.contextCollapsed);
  elements.toggleContextButton.textContent = state.contextCollapsed ? '◰' : '◱';
  elements.toggleContextButton.title = state.contextCollapsed ? '打开详情' : '收起详情';
  document.querySelectorAll('.context-tab').forEach((tab) => {
    tab.classList.toggle('active', tab.dataset.contextTab === state.activeContextTab);
  });
  if (!elements.contextContent) {
    return;
  }
  if (state.activeContextTab === 'detail') {
    renderDetailContext();
  } else {
    renderOverviewContext();
  }
}

function renderOverviewContext() {
  const status = selectedStatus();
  const conversation = currentConversation();
  if (!state.selected) {
    elements.contextContent.innerHTML = `
      <div class="context-empty">
        <strong>没有选中会话</strong>
        <span>从左侧选择一个 Conversation。</span>
      </div>
    `;
    return;
  }
  const usage = statusUsageTotals(status);
  const remote = isRemoteStatus(status);
  const workspace = status?.workspace || 'workspace pending';
  const crumbs = compactPath(workspace).split('/').filter(Boolean);
  elements.contextContent.innerHTML = `
    <section class="inspector-card hero-card">
      <div class="inspector-kicker">${escapeHtml(conversation?.conversation_id || state.selected.conversationId)}</div>
      <h2>${escapeHtml(conversation ? displayConversationName(state.selected.serverId, conversation) : state.selected.conversationId)}</h2>
      <div class="status-line">
        <span class="status-dot ${remote ? 'remote' : ''}"></span>
        <span>${remote ? escapeHtml(status.remote) : 'local workspace'}</span>
      </div>
    </section>
    <section class="metric-grid">
      <div class="metric-card">
        <span>Cache</span>
        <strong>${Math.round(usage.cacheHit * 100)}%</strong>
      </div>
      <div class="metric-card">
        <span>Tokens</span>
        <strong>${formatCompactNumber(usage.totalTokens)}</strong>
      </div>
      <div class="metric-card">
        <span>Cost</span>
        <strong>${formatCost(usage.cost)}</strong>
      </div>
    </section>
    <section class="inspector-card">
      <div class="inspector-title">运行状态</div>
      <div class="kv-list">
        <span>model</span><strong>${escapeHtml(status?.model || conversation?.model || 'pending')}</strong>
        <span>sandbox</span><strong>${escapeHtml(status?.sandbox || 'pending')}</strong>
        <span>background</span><strong>${Number(status?.running_background || 0)} / ${Number(status?.total_background || 0)}</strong>
        <span>subagents</span><strong>${Number(status?.running_subagents || 0)} / ${Number(status?.total_subagents || 0)}</strong>
      </div>
    </section>
    <section class="inspector-card">
      <div class="inspector-title">Workspace</div>
      <div class="path-chip">${escapeHtml(workspace)}</div>
      <div class="crumb-row">${crumbs.map((part) => `<span>${escapeHtml(part)}</span>`).join('')}</div>
    </section>
    <section class="inspector-card">
      <div class="inspector-title">Usage</div>
      ${usageBar('Cache Read', usage.cacheRead, usage.totalTokens)}
      ${usageBar('Cache Write', usage.cacheWrite, usage.totalTokens)}
      ${usageBar('Input', usage.input, usage.totalTokens)}
      ${usageBar('Output', usage.output, usage.totalTokens)}
    </section>
  `;
}

function usageBar(label, value, total) {
  const percent = total > 0 ? Math.max(3, Math.round((value / total) * 100)) : 0;
  return `
    <div class="usage-row">
      <div class="usage-row-head"><span>${escapeHtml(label)}</span><strong>${formatCompactNumber(value)}</strong></div>
      <div class="usage-track"><span style="width: ${percent}%"></span></div>
    </div>
  `;
}

function renderDetailContext() {
  const messageId = state.activePreviewMessageId;
  const message = state.messages.find((item) => item.id === messageId);
  const detail = messageId ? state.messageDetails.get(messageId) : null;
  if (!message) {
    elements.contextContent.innerHTML = `
      <div class="context-empty">
        <strong>未选择消息</strong>
        <span>点击中间消息查看细节。</span>
      </div>
    `;
    return;
  }
  const rendered = detail?.rendered_text || message.preview || '';
  elements.contextContent.innerHTML = `
    <section class="inspector-card hero-card">
      <div class="inspector-kicker">${escapeHtml(message.role || 'message')}</div>
      <h2>${escapeHtml(message.user_name || message.role || 'assistant')}</h2>
      <div class="status-line">
        <span class="status-dot"></span>
        <span>${escapeHtml(message.message_time ? formatRelative(message.message_time) : 'no timestamp')}</span>
      </div>
    </section>
    <section class="preview-card">
      ${renderMarkdownMessage(rendered)}
    </section>
  `;
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
  renderContext();
  await refreshAllServers();
}

function bindEvents() {
  elements.newConversationButton.addEventListener('click', openNewConversationModal);
  elements.settingsButton.addEventListener('click', openSettingsModal);
  elements.refreshButton.addEventListener('click', refreshConversation);
  elements.toggleContextButton.addEventListener('click', () => {
    state.contextCollapsed = !state.contextCollapsed;
    renderContext();
  });
  elements.sendButton.addEventListener('click', sendMessage);
  elements.attachButton.addEventListener('click', () => showToast('文件上下文会跟随后端 API 一起接入。'));
  elements.collapseContextButton.addEventListener('click', () => {
    state.contextCollapsed = true;
    renderContext();
  });
  document.querySelectorAll('.context-tab').forEach((tab) => {
    tab.addEventListener('click', () => {
      state.activeContextTab = tab.dataset.contextTab;
      renderContext();
    });
  });
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
