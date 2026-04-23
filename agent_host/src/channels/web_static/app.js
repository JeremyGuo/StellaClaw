// ClawParty Web client
(function () {
  'use strict';

  const SERVER_KEY = 'clawparty_server_base';
  const AUTH_KEY = 'clawparty_auth_token';
  const CONVERSATION_KEY = 'clawparty_conversation_key';
  const WORKSPACE_DRAFT_KEY = 'clawparty_workspace_draft';
  const PAGE_SIZE = 30;

  const messagesEl = document.getElementById('messages');
  const inputEl = document.getElementById('input');
  const formEl = document.getElementById('input-form');
  const statusEl = document.getElementById('connection-status');
  const conversationListEl = document.getElementById('conversation-list');
  const conversationTitleEl = document.getElementById('conversation-title');
  const conversationSubtitleEl = document.getElementById('conversation-subtitle');
  const heroConversationEl = document.getElementById('hero-conversation');
  const heroEl = document.getElementById('hero');
  const mainEl = document.getElementById('main');
  const newConversationBtn = document.getElementById('new-conversation-btn');
  const refreshEl = document.getElementById('refresh-btn');
  const applyWorkspaceBtn = document.getElementById('apply-workspace-btn');
  const settingsBtn = document.getElementById('settings-btn');
  const sidebarSettingsBtn = document.getElementById('sidebar-settings-btn');
  const heroConfigureBtn = document.getElementById('hero-configure-btn');
  const heroRefreshBtn = document.getElementById('hero-refresh-btn');
  const workspacePillEl = document.getElementById('workspace-pill');
  const serverPillEl = document.getElementById('server-pill');
  const settingsBackdropEl = document.getElementById('settings-backdrop');
  const settingsDrawerEl = document.getElementById('settings-drawer');
  const closeSettingsBtn = document.getElementById('close-settings-btn');
  const serverUrlInputEl = document.getElementById('server-url-input');
  const authTokenInputEl = document.getElementById('auth-token-input');
  const workspaceKindInputEl = document.getElementById('workspace-kind-input');
  const localPathFieldEl = document.getElementById('local-path-field');
  const sshHostFieldEl = document.getElementById('ssh-host-field');
  const sshPathFieldEl = document.getElementById('ssh-path-field');
  const workspaceLocalPathInputEl = document.getElementById('workspace-local-path-input');
  const workspaceSshHostInputEl = document.getElementById('workspace-ssh-host-input');
  const workspaceSshPathInputEl = document.getElementById('workspace-ssh-path-input');
  const saveServerSettingsBtn = document.getElementById('save-server-settings-btn');
  const saveWorkspaceDraftBtn = document.getElementById('save-workspace-draft-btn');
  const bindWorkspaceBtn = document.getElementById('bind-workspace-btn');
  const currentConversationSummaryEl = document.getElementById('current-conversation-summary');

  let ws = null;
  let reconnectTimer = null;
  let serverBase = normalizeServerBase(localStorage.getItem(SERVER_KEY) || defaultServerBase());
  let token = localStorage.getItem(AUTH_KEY) || '';
  let currentConversation = (
    new URLSearchParams(window.location.search).get('conversation') ||
    localStorage.getItem(CONVERSATION_KEY) ||
    ''
  );
  let currentConversationSummary = null;
  let transcriptOffset = 0;
  let hasMoreTranscript = true;
  let loadMoreEl = null;
  let loadingOlder = false;
  let conversationCache = [];
  let autoStickToBottom = true;
  let composingInput = false;
  const renderedSeqs = new Set();

  function defaultServerBase() {
    if (window.location.protocol === 'file:') {
      return 'http://127.0.0.1:8080';
    }
    return window.location.origin;
  }

  function normalizeServerBase(raw) {
    const value = String(raw || '').trim();
    if (!value) return defaultServerBase();
    try {
      const url = new URL(value, window.location.origin);
      if (url.protocol === 'file:') {
        return defaultServerBase();
      }
      return url.href.replace(/\/+$/, '');
    } catch (e) {
      return defaultServerBase();
    }
  }

  function saveServerSettings() {
    serverBase = normalizeServerBase(serverUrlInputEl.value);
    token = authTokenInputEl.value.trim();
    persistServerSettingsDraft();
    updateServerPill();
  }

  function persistServerSettingsDraft() {
    const draftServerBase = normalizeServerBase(serverUrlInputEl.value);
    const draftToken = authTokenInputEl.value.trim();
    localStorage.setItem(SERVER_KEY, draftServerBase);
    if (draftToken) {
      localStorage.setItem(AUTH_KEY, draftToken);
    } else {
      localStorage.removeItem(AUTH_KEY);
    }
  }

  function saveWorkspaceDraft() {
    const draft = {
      kind: workspaceKindInputEl.value === 'ssh' ? 'ssh' : 'local',
      localPath: workspaceLocalPathInputEl.value.trim(),
      sshHost: workspaceSshHostInputEl.value.trim(),
      sshPath: workspaceSshPathInputEl.value.trim(),
    };
    localStorage.setItem(WORKSPACE_DRAFT_KEY, JSON.stringify(draft));
    updateWorkspaceFieldVisibility();
    updateWorkspacePill();
  }

  function loadWorkspaceDraft() {
    try {
      const draft = JSON.parse(localStorage.getItem(WORKSPACE_DRAFT_KEY) || '{}');
      workspaceKindInputEl.value = draft.kind === 'ssh' ? 'ssh' : 'local';
      workspaceLocalPathInputEl.value = draft.localPath || '';
      workspaceSshHostInputEl.value = draft.sshHost || '';
      workspaceSshPathInputEl.value = draft.sshPath || '';
    } catch (e) {
      workspaceKindInputEl.value = 'local';
      workspaceLocalPathInputEl.value = '';
      workspaceSshHostInputEl.value = '';
      workspaceSshPathInputEl.value = '';
    }
    updateWorkspaceFieldVisibility();
  }

  function applyRemoteExecutionToInputs(remoteExecution) {
    if (!remoteExecution || !remoteExecution.kind) return;
    if (remoteExecution.kind === 'ssh') {
      workspaceKindInputEl.value = 'ssh';
      workspaceLocalPathInputEl.value = '';
      workspaceSshHostInputEl.value = remoteExecution.host || '';
      workspaceSshPathInputEl.value = remoteExecution.path || '';
    } else {
      workspaceKindInputEl.value = 'local';
      workspaceLocalPathInputEl.value = remoteExecution.path || '';
      workspaceSshHostInputEl.value = '';
      workspaceSshPathInputEl.value = '';
    }
    updateWorkspaceFieldVisibility();
  }

  function updateWorkspaceFieldVisibility() {
    const useSsh = workspaceKindInputEl.value === 'ssh';
    localPathFieldEl.hidden = useSsh;
    sshHostFieldEl.hidden = !useSsh;
    sshPathFieldEl.hidden = !useSsh;
  }

  function draftWorkspaceLabel() {
    if (workspaceKindInputEl.value === 'ssh') {
      const host = workspaceSshHostInputEl.value.trim();
      const path = workspaceSshPathInputEl.value.trim();
      return host && path ? host + ' ' + path : 'Draft not ready';
    }
    const path = workspaceLocalPathInputEl.value.trim();
    return path || 'Draft not ready';
  }

  function conversationWorkspaceLabel(conversation) {
    return (conversation && conversation.remote_execution_label) || null;
  }

  function updateWorkspacePill() {
    const label = conversationWorkspaceLabel(currentConversationSummary) || draftWorkspaceLabel();
    workspacePillEl.textContent = label || 'Not bound';
    workspacePillEl.classList.toggle('muted', !conversationWorkspaceLabel(currentConversationSummary));
  }

  function updateServerPill() {
    serverPillEl.textContent = serverBase.replace(/^https?:\/\//, '').replace(/^wss?:\/\//, '');
  }

  function updateConversationHeader() {
    const fallback = currentConversation || 'Untitled conversation';
    conversationTitleEl.textContent = fallback;
    heroConversationEl.textContent = fallback;
    if (currentConversationSummary && currentConversationSummary.remote_execution_label) {
      conversationSubtitleEl.textContent =
        'Execution root: ' + currentConversationSummary.remote_execution_label;
    } else {
      conversationSubtitleEl.textContent =
        'Configure a remote execution root before sending the first message';
    }
    updateCurrentConversationCard();
    updateWorkspacePill();
  }

  function updateCurrentConversationCard() {
    if (!currentConversation) {
      currentConversationSummaryEl.textContent = 'No conversation selected.';
      return;
    }
    if (!currentConversationSummary) {
      currentConversationSummaryEl.textContent =
        'This conversation has not been created on the server yet. Bind a workspace to create it.';
      return;
    }
    const lines = [
      'Conversation: ' + currentConversationSummary.conversation_key,
      'Messages: ' + currentConversationSummary.entry_count,
      'Workspace: ' + (currentConversationSummary.remote_execution_label || 'Not bound'),
    ];
    if (currentConversationSummary.latest_summary) {
      lines.push('Latest: ' + currentConversationSummary.latest_summary);
    }
    currentConversationSummaryEl.textContent = lines.join('\n');
  }

  function updateHeroState() {
    const hasRenderableMessages = Array.from(messagesEl.children).some(function (node) {
      return node !== loadMoreEl;
    });
    heroEl.hidden = hasRenderableMessages;
  }

  function setConversation(key) {
    currentConversation = key;
    currentConversationSummary = null;
    localStorage.setItem(CONVERSATION_KEY, currentConversation);
    const url = new URL(window.location.href);
    url.searchParams.set('conversation', currentConversation);
    window.history.replaceState(null, '', url);
    updateConversationHeader();
    renderConversationList(conversationCache);
  }

  function generateConversationKey() {
    return 'web-' + Math.random().toString(36).slice(2, 10) + Date.now().toString(36).slice(-4);
  }

  function openSettings() {
    settingsBackdropEl.hidden = false;
    settingsDrawerEl.hidden = false;
  }

  function closeSettings() {
    settingsBackdropEl.hidden = true;
    settingsDrawerEl.hidden = true;
  }

  function authHeaders() {
    const headers = { 'Content-Type': 'application/json' };
    if (token) headers.Authorization = 'Bearer ' + token;
    return headers;
  }

  function apiUrl(path, params) {
    const url = new URL(path, serverBase + '/');
    if (params) {
      Object.keys(params).forEach(function (key) {
        const value = params[key];
        if (value != null && value !== '') url.searchParams.set(key, value);
      });
    }
    return url;
  }

  function wsUrl() {
    const http = new URL('/ws', serverBase + '/');
    http.protocol = http.protocol === 'https:' ? 'wss:' : 'ws:';
    if (token) http.searchParams.set('token', token);
    return http.toString();
  }

  async function parseError(resp) {
    const text = (await resp.text()).trim();
    return text || (resp.status + ' ' + resp.statusText);
  }

  async function apiRequest(method, path, body, params) {
    const resp = await fetch(apiUrl(path, params), {
      method: method,
      headers: authHeaders(),
      body: body == null ? undefined : JSON.stringify(body),
    });
    if (!resp.ok) {
      throw new Error(await parseError(resp));
    }
    if (resp.status === 204) return null;
    return resp.json();
  }

  function apiGet(path, params) {
    return apiRequest('GET', path, null, params);
  }

  function apiPost(path, body) {
    return apiRequest('POST', path, body);
  }

  function apiPut(path, body) {
    return apiRequest('PUT', path, body);
  }

  function apiDelete(path, body) {
    return apiRequest('DELETE', path, body);
  }

  function normalizeConversationList(conversations) {
    const items = (conversations || []).slice();
    if (currentConversation && !items.some(function (item) { return item.conversation_key === currentConversation; })) {
      items.unshift({
        conversation_key: currentConversation,
        entry_count: 0,
        latest_summary: 'Bind a workspace to create this conversation',
        remote_execution: null,
        remote_execution_label: null,
      });
    }
    return items;
  }

  async function loadConversations() {
    conversationCache = normalizeConversationList(await apiGet('/api/conversations'));
    if (!currentConversation) {
      currentConversation = conversationCache[0]
        ? conversationCache[0].conversation_key
        : generateConversationKey();
      localStorage.setItem(CONVERSATION_KEY, currentConversation);
    }
    renderConversationList(conversationCache);
    updateConversationHeader();
  }

  async function loadConversationState() {
    if (!currentConversation) return null;
    try {
      currentConversationSummary = await apiGet('/api/conversation', {
        conversation_key: currentConversation,
      });
      if (currentConversationSummary && currentConversationSummary.remote_execution) {
        applyRemoteExecutionToInputs(currentConversationSummary.remote_execution);
        saveWorkspaceDraft();
      }
    } catch (error) {
      if (String(error.message).indexOf('404') !== -1 || String(error.message).indexOf('Not Found') !== -1) {
        currentConversationSummary = null;
      } else {
        throw error;
      }
    }
    updateConversationHeader();
    return currentConversationSummary;
  }

  function serializeWorkspaceDraft() {
    saveWorkspaceDraft();
    if (workspaceKindInputEl.value === 'ssh') {
      const host = workspaceSshHostInputEl.value.trim();
      const path = workspaceSshPathInputEl.value.trim();
      if (!host || !path) {
        throw new Error('SSH workspace requires both host and remote path');
      }
      return { kind: 'ssh', host: host, path: path };
    }
    const path = workspaceLocalPathInputEl.value.trim();
    if (!path) {
      throw new Error('Local workspace requires an absolute path');
    }
    return { kind: 'local', path: path };
  }

  async function bindCurrentConversation(forceUpdate) {
    if (!currentConversation) {
      setConversation(generateConversationKey());
    }
    const remoteExecution = serializeWorkspaceDraft();
    const body = {
      conversation_key: currentConversation,
      remote_execution: remoteExecution,
    };
    currentConversationSummary = forceUpdate || currentConversationSummary
      ? await apiPut('/api/conversation', body)
      : await apiPost('/api/conversation', body);
    conversationCache = normalizeConversationList(
      [currentConversationSummary].concat(
        conversationCache.filter(function (item) {
          return item.conversation_key !== currentConversationSummary.conversation_key;
        })
      )
    );
    renderConversationList(conversationCache);
    updateConversationHeader();
    return currentConversationSummary;
  }

  async function ensureConversationConfigured() {
    await loadConversationState();
    if (currentConversationSummary && currentConversationSummary.remote_execution) {
      return currentConversationSummary;
    }
    openSettings();
    return bindCurrentConversation(Boolean(currentConversationSummary));
  }

  function renderConversationList(conversations) {
    conversationListEl.textContent = '';
    conversations.forEach(function (conversation) {
      const key = conversation.conversation_key;
      const row = document.createElement('div');
      row.className = 'conversation-row' + (key === currentConversation ? ' active' : '');

      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'conversation-item';
      button.title = key;
      button.addEventListener('click', function () {
        switchConversation(key);
      });

      const name = document.createElement('div');
      name.className = 'conversation-name';
      name.textContent = key;
      button.appendChild(name);

      const root = document.createElement('div');
      root.className = 'conversation-root';
      root.textContent = conversation.remote_execution_label || 'Remote workspace not bound';
      button.appendChild(root);

      const summary = document.createElement('div');
      summary.className = 'conversation-summary';
      summary.textContent = conversation.latest_summary || 'No transcript yet';
      button.appendChild(summary);

      const del = document.createElement('button');
      del.type = 'button';
      del.className = 'conversation-delete';
      del.textContent = 'Delete';
      del.title = 'Delete conversation';
      del.addEventListener('click', function (event) {
        event.stopPropagation();
        deleteConversation(key);
      });

      row.appendChild(button);
      row.appendChild(del);
      conversationListEl.appendChild(row);
    });
  }

  async function switchConversation(key) {
    if (!key || key === currentConversation) return;
    setConversation(key);
    resetTranscript();
    await loadConversationState();
    await loadTranscriptPage('latest');
  }

  async function createConversation() {
    setConversation(generateConversationKey());
    resetTranscript();
    updateHeroState();
    openSettings();
    inputEl.focus();
  }

  async function deleteConversation(key) {
    if (!key) return;
    if (!window.confirm('Delete conversation "' + key + '"?')) return;
    await apiDelete('/api/conversation', { conversation_key: key });
    const wasCurrent = key === currentConversation;
    conversationCache = conversationCache.filter(function (item) {
      return item.conversation_key !== key;
    });
    if (!wasCurrent) {
      renderConversationList(conversationCache);
      return;
    }
    const next = conversationCache[0];
    if (next) {
      await switchConversation(next.conversation_key);
    } else {
      await createConversation();
    }
  }

  function appendMessage(role, text, meta, options, attachments) {
    const shouldStick = autoStickToBottom || isNearBottom(160);
    const div = document.createElement('div');
    div.className = 'msg ' + role;
    if (meta) {
      const metaEl = document.createElement('div');
      metaEl.className = 'meta';
      metaEl.textContent = meta;
      div.appendChild(metaEl);
    }
    const body = document.createElement('div');
    renderMessageBody(body, role, text || '');
    div.appendChild(body);
    appendAttachments(div, attachments || []);
    appendOptions(div, options);
    messagesEl.appendChild(div);
    updateHeroState();
    if (shouldStick) scrollToBottom();
    return div;
  }

  function appendOptions(parent, options) {
    if (!options || !options.options || !options.options.length) return;

    const prompt = document.createElement('div');
    prompt.className = 'options-prompt';
    prompt.textContent = options.prompt || 'Choose one';
    parent.appendChild(prompt);

    const buttons = document.createElement('div');
    buttons.className = 'options';
    options.options.forEach(function (option) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'option-btn';
      button.textContent = option.label || option.value;
      button.addEventListener('click', function () {
        buttons.querySelectorAll('button').forEach(function (item) {
          item.disabled = true;
        });
        sendMessage(option.value);
      });
      buttons.appendChild(button);
    });
    parent.appendChild(buttons);
  }

  function appendEvent(text) {
    return appendMessage('event', text);
  }

  function renderMessageBody(parent, role, text) {
    parent.className = 'message-body';
    if (role === 'assistant') {
      parent.classList.add('markdown');
      renderAssistantContent(parent, text);
    } else {
      parent.textContent = text;
    }
  }

  function renderAssistantContent(parent, text) {
    parent.textContent = '';
    splitAttachmentTags(text).forEach(function (part) {
      if (part.type === 'text') {
        const container = document.createElement('div');
        renderMarkdown(container, part.value);
        while (container.firstChild) parent.appendChild(container.firstChild);
      } else {
        parent.appendChild(buildAttachmentElement({
          source: 'workspace',
          path: part.value,
          kind: guessAttachmentKind(part.value),
        }));
      }
    });
  }

  function splitAttachmentTags(text) {
    const parts = [];
    const source = String(text || '');
    const regex = /<attachment>([^<]+)<\/attachment>/g;
    let lastIndex = 0;
    let match;
    while ((match = regex.exec(source)) !== null) {
      if (match.index > lastIndex) {
        parts.push({ type: 'text', value: source.slice(lastIndex, match.index) });
      }
      parts.push({ type: 'attachment', value: match[1].trim() });
      lastIndex = regex.lastIndex;
    }
    if (lastIndex < source.length) {
      parts.push({ type: 'text', value: source.slice(lastIndex) });
    }
    return parts.length ? parts : [{ type: 'text', value: source }];
  }

  function renderMarkdown(parent, text) {
    parent.textContent = '';
    const lines = String(text || '').replace(/\r\n/g, '\n').split('\n');
    let index = 0;

    while (index < lines.length) {
      const line = lines[index];
      if (!line.trim()) {
        index += 1;
        continue;
      }

      const fence = line.match(/^```([A-Za-z0-9_-]+)?\s*$/);
      if (fence) {
        const codeLines = [];
        index += 1;
        while (index < lines.length && !/^```\s*$/.test(lines[index])) {
          codeLines.push(lines[index]);
          index += 1;
        }
        if (index < lines.length) index += 1;
        const pre = document.createElement('pre');
        const code = document.createElement('code');
        if (fence[1]) code.dataset.lang = fence[1];
        code.textContent = codeLines.join('\n');
        pre.appendChild(code);
        parent.appendChild(pre);
        continue;
      }

      const heading = line.match(/^(#{1,6})\s+(.+)$/);
      if (heading) {
        const node = document.createElement('h' + Math.min(heading[1].length, 6));
        renderInlineMarkdown(node, heading[2]);
        parent.appendChild(node);
        index += 1;
        continue;
      }

      if (isMarkdownHorizontalRule(line)) {
        parent.appendChild(document.createElement('hr'));
        index += 1;
        continue;
      }

      if (isMarkdownTableStart(lines, index)) {
        const rendered = renderMarkdownTable(lines, index);
        parent.appendChild(rendered.node);
        index = rendered.nextIndex;
        continue;
      }

      if (/^>\s?/.test(line)) {
        const quote = document.createElement('blockquote');
        while (index < lines.length && /^>\s?/.test(lines[index])) {
          const paragraph = document.createElement('p');
          renderInlineMarkdown(paragraph, lines[index].replace(/^>\s?/, ''));
          quote.appendChild(paragraph);
          index += 1;
        }
        parent.appendChild(quote);
        continue;
      }

      const listMatch = line.match(/^(\s*)([-*+]|\d+\.)\s+(.+)$/);
      if (listMatch) {
        const ordered = /\d+\./.test(listMatch[2]);
        const list = document.createElement(ordered ? 'ol' : 'ul');
        while (index < lines.length) {
          const item = lines[index].match(/^(\s*)([-*+]|\d+\.)\s+(.+)$/);
          if (!item || /\d+\./.test(item[2]) !== ordered) break;
          const li = document.createElement('li');
          renderInlineMarkdown(li, item[3]);
          list.appendChild(li);
          index += 1;
        }
        parent.appendChild(list);
        continue;
      }

      const paragraphLines = [];
      while (index < lines.length && lines[index].trim() && !isMarkdownBlockStartAt(lines, index)) {
        paragraphLines.push(lines[index]);
        index += 1;
      }
      const p = document.createElement('p');
      renderInlineMarkdown(p, paragraphLines.join(' '));
      parent.appendChild(p);
    }
  }

  function isMarkdownBlockStartAt(lines, index) {
    const line = lines[index];
    return /^```/.test(line) ||
      /^(#{1,6})\s+/.test(line) ||
      isMarkdownHorizontalRule(line) ||
      isMarkdownTableStart(lines, index) ||
      /^>\s?/.test(line) ||
      /^(\s*)([-*+]|\d+\.)\s+/.test(line);
  }

  function isMarkdownHorizontalRule(line) {
    return /^\s{0,3}(?:-{3,}|\*{3,}|_{3,})\s*$/.test(line);
  }

  function renderMarkdownTable(lines, startIndex) {
    const headerCells = splitMarkdownTableRow(lines[startIndex]);
    const delimiterCells = splitMarkdownTableRow(lines[startIndex + 1]);
    const alignments = delimiterCells.map(tableAlignment);
    const rows = [];
    let index = startIndex + 2;
    while (index < lines.length && lines[index].trim() && hasUnescapedPipe(lines[index])) {
      if (isMarkdownTableDelimiter(lines[index])) break;
      rows.push(splitMarkdownTableRow(lines[index]));
      index += 1;
    }

    const width = Math.max(
      headerCells.length,
      delimiterCells.length,
      rows.reduce(function (max, row) { return Math.max(max, row.length); }, 0)
    );
    const wrap = document.createElement('div');
    wrap.className = 'markdown-table-wrap';
    const table = document.createElement('table');
    const thead = document.createElement('thead');
    const headRow = document.createElement('tr');
    for (let col = 0; col < width; col += 1) {
      const th = document.createElement('th');
      applyTableAlignment(th, alignments[col]);
      renderInlineMarkdown(th, headerCells[col] || '');
      headRow.appendChild(th);
    }
    thead.appendChild(headRow);
    table.appendChild(thead);

    const tbody = document.createElement('tbody');
    rows.forEach(function (row) {
      const tr = document.createElement('tr');
      for (let col = 0; col < width; col += 1) {
        const td = document.createElement('td');
        applyTableAlignment(td, alignments[col]);
        renderInlineMarkdown(td, row[col] || '');
        tr.appendChild(td);
      }
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    wrap.appendChild(table);
    return { node: wrap, nextIndex: index };
  }

  function isMarkdownTableStart(lines, index) {
    if (index + 1 >= lines.length) return false;
    if (!hasUnescapedPipe(lines[index])) return false;
    if (!isMarkdownTableDelimiter(lines[index + 1])) return false;
    return splitMarkdownTableRow(lines[index]).length >= 2;
  }

  function isMarkdownTableDelimiter(line) {
    if (!hasUnescapedPipe(line)) return false;
    const cells = splitMarkdownTableRow(line);
    if (cells.length < 2) return false;
    return cells.every(function (cell) {
      return /^:?-{3,}:?$/.test(cell.trim());
    });
  }

  function splitMarkdownTableRow(line) {
    const cells = [];
    let current = '';
    const source = String(line || '').trim();
    for (let i = 0; i < source.length; i += 1) {
      const ch = source[i];
      if (ch === '\\' && i + 1 < source.length) {
        current += source[i + 1];
        i += 1;
      } else if (ch === '|') {
        cells.push(current.trim());
        current = '';
      } else {
        current += ch;
      }
    }
    cells.push(current.trim());
    if (source.startsWith('|') && cells[0] === '') cells.shift();
    if (source.endsWith('|') && cells[cells.length - 1] === '') cells.pop();
    return cells;
  }

  function hasUnescapedPipe(line) {
    const source = String(line || '');
    for (let i = 0; i < source.length; i += 1) {
      if (source[i] === '\\') {
        i += 1;
      } else if (source[i] === '|') {
        return true;
      }
    }
    return false;
  }

  function tableAlignment(cell) {
    const value = String(cell || '').trim();
    const left = value.startsWith(':');
    const right = value.endsWith(':');
    if (left && right) return 'center';
    if (right) return 'right';
    return null;
  }

  function applyTableAlignment(node, alignment) {
    if (alignment) node.style.textAlign = alignment;
  }

  function renderInlineMarkdown(parent, text) {
    const source = String(text || '');
    let index = 0;
    while (index < source.length) {
      const match = nextInlineMatch(source, index);
      if (!match) {
        parent.appendChild(document.createTextNode(source.slice(index)));
        break;
      }
      if (match.start > index) {
        parent.appendChild(document.createTextNode(source.slice(index, match.start)));
      }
      appendInlineNode(parent, match);
      index = match.end;
    }
  }

  function nextInlineMatch(source, startIndex) {
    const candidates = [
      { type: 'code', regex: /`([^`]+)`/g },
      { type: 'link', regex: /\[([^\]]+)\]\(([^)\s]+)\)/g },
      { type: 'bold', regex: /\*\*([^*]+)\*\*/g },
      { type: 'italic', regex: /\*([^*]+)\*/g },
    ];
    let best = null;
    candidates.forEach(function (candidate) {
      candidate.regex.lastIndex = startIndex;
      const match = candidate.regex.exec(source);
      if (!match) return;
      if (!best || match.index < best.start) {
        best = {
          type: candidate.type,
          start: match.index,
          end: candidate.regex.lastIndex,
          match: match,
        };
      }
    });
    return best;
  }

  function appendInlineNode(parent, match) {
    if (match.type === 'code') {
      const code = document.createElement('code');
      code.textContent = match.match[1];
      parent.appendChild(code);
    } else if (match.type === 'link') {
      const link = document.createElement('a');
      link.href = safeHref(match.match[2]);
      link.target = '_blank';
      link.rel = 'noopener noreferrer';
      renderInlineMarkdown(link, match.match[1]);
      parent.appendChild(link);
    } else if (match.type === 'bold') {
      const strong = document.createElement('strong');
      renderInlineMarkdown(strong, match.match[1]);
      parent.appendChild(strong);
    } else if (match.type === 'italic') {
      const em = document.createElement('em');
      renderInlineMarkdown(em, match.match[1]);
      parent.appendChild(em);
    }
  }

  function safeHref(raw) {
    try {
      const url = new URL(raw, window.location.href);
      if (url.protocol === 'http:' || url.protocol === 'https:' || url.protocol === 'mailto:') {
        return url.href;
      }
    } catch (e) {
      return '#';
    }
    return '#';
  }

  function appendAttachments(parent, attachments) {
    if (!attachments || !attachments.length) return;
    const wrap = document.createElement('div');
    wrap.className = 'attachments';
    attachments.forEach(function (attachment) {
      wrap.appendChild(buildAttachmentElement(attachment));
    });
    parent.appendChild(wrap);
  }

  function buildAttachmentElement(attachment) {
    const source = attachment.source || 'workspace';
    const path = attachment.path || '';
    const kind = attachment.kind || guessAttachmentKind(path);
    const url = attachmentUrl(source, path);
    const figure = document.createElement('figure');
    figure.className = 'attachment';
    if (isImageAttachment(kind, path)) {
      const image = document.createElement('img');
      image.src = url;
      image.alt = attachment.caption || path;
      image.loading = 'lazy';
      image.addEventListener('load', function () {
        if (autoStickToBottom || isNearBottom(160)) scrollToBottom();
      });
      figure.appendChild(image);
    } else {
      const link = document.createElement('a');
      link.href = url;
      link.target = '_blank';
      link.rel = 'noopener noreferrer';
      link.textContent = path;
      figure.appendChild(link);
    }
    if (attachment.caption) {
      const caption = document.createElement('figcaption');
      caption.textContent = attachment.caption;
      figure.appendChild(caption);
    }
    return figure;
  }

  function attachmentUrl(source, path) {
    const url = apiUrl('/api/attachment', {
      conversation_key: currentConversation,
      source: source,
      path: path,
    });
    if (token) url.searchParams.set('token', token);
    return url.toString();
  }

  function guessAttachmentKind(path) {
    return /\.(png|jpe?g|gif|webp|avif|apng|svg)$/i.test(path) ? 'Image' : 'File';
  }

  function isImageAttachment(kind, path) {
    return String(kind).toLowerCase() === 'image' || guessAttachmentKind(path) === 'Image';
  }

  async function sendMessage(text) {
    try {
      await ensureConversationConfigured();
      await apiPost('/api/send', {
        text: text,
        conversation_key: currentConversation,
      });
      await loadConversations();
      window.setTimeout(function () {
        loadTranscriptPage('latest');
      }, 250);
    } catch (e) {
      appendEvent('Send failed: ' + e.message);
    }
  }

  function resetTranscript() {
    messagesEl.textContent = '';
    renderedSeqs.clear();
    transcriptOffset = 0;
    hasMoreTranscript = true;
    loadingOlder = false;
    loadMoreEl = document.createElement('button');
    loadMoreEl.type = 'button';
    loadMoreEl.className = 'load-more';
    loadMoreEl.textContent = 'Load older';
    loadMoreEl.addEventListener('click', function () {
      loadTranscriptPage('older');
    });
    messagesEl.appendChild(loadMoreEl);
    updateHeroState();
  }

  async function loadTranscriptPage(mode) {
    if (!currentConversation) return;
    if (mode === 'older' && !hasMoreTranscript) return;
    if (mode === 'older' && loadingOlder) return;
    if (mode === 'older') loadingOlder = true;
    const previousHeight = messagesEl.scrollHeight;
    const previousTop = messagesEl.scrollTop;
    const offset = mode === 'older' ? transcriptOffset : 0;
    try {
      const newestFirst = await apiGet('/api/transcript', {
        conversation_key: currentConversation,
        offset: String(offset),
        limit: String(PAGE_SIZE),
      });
      const chronological = newestFirst.slice().reverse();
      const entriesToRender = mode === 'older' ? chronological.slice().reverse() : chronological;
      entriesToRender.forEach(function (entry) {
        renderSkeleton(entry, mode === 'older' ? 'prepend' : 'append');
      });
      if (mode === 'older') transcriptOffset += newestFirst.length;
      if (mode !== 'older') transcriptOffset = Math.max(transcriptOffset, newestFirst.length);
      hasMoreTranscript = newestFirst.length === PAGE_SIZE;
      updateLoadMore();
      updateHeroState();
      if (mode === 'older') {
        messagesEl.scrollTop = previousTop + (messagesEl.scrollHeight - previousHeight);
      } else {
        scrollToBottom();
      }
    } catch (e) {
      if (mode !== 'older') updateLoadMore();
      appendEvent('Transcript load failed: ' + e.message);
    } finally {
      if (mode === 'older') loadingOlder = false;
    }
  }

  function updateLoadMore() {
    if (!loadMoreEl) return;
    loadMoreEl.disabled = !hasMoreTranscript;
    loadMoreEl.textContent = hasMoreTranscript ? 'Load older' : 'No older transcript';
  }

  function renderSkeleton(entry, mode) {
    if (isHiddenUserTellResult(entry)) return;
    if (renderedSeqs.has(entry.seq)) return;
    renderedSeqs.add(entry.seq);

    let element;
    if (entry.type === 'user_message') {
      element = buildMessageElement('user', entry.text || '(empty)', metaFor(entry), entry.options);
    } else if (entry.type === 'assistant_message') {
      element = buildMessageElement('assistant', entry.text || '', metaFor(entry), entry.options);
    } else if (isUserTellModelCall(entry)) {
      element = buildMessageElement('assistant', entry.user_tell_text_preview || '', modelCallMetaFor(entry), null);
    } else if (isAssistantModelCall(entry)) {
      element = buildHistoricalAssistantElement(entry);
    } else {
      element = buildTraceElement(entry);
    }
    element.dataset.seq = String(entry.seq);
    insertTranscriptElement(element, mode);
    updateHeroState();
  }

  function buildMessageElement(role, text, meta, options) {
    const div = document.createElement('div');
    div.className = 'msg ' + role;
    if (meta) {
      const metaEl = document.createElement('div');
      metaEl.className = 'meta';
      metaEl.textContent = meta;
      div.appendChild(metaEl);
    }
    const body = document.createElement('div');
    renderMessageBody(body, role, text);
    div.appendChild(body);
    appendOptions(div, options);
    return div;
  }

  function isAssistantModelCall(entry) {
    return entry.type === 'model_call' &&
      entry.assistant_text_preview &&
      (!entry.tool_call_names || entry.tool_call_names.length === 0);
  }

  function isUserTellModelCall(entry) {
    return entry.type === 'model_call' &&
      entry.user_tell_text_preview &&
      entry.tool_call_names &&
      entry.tool_call_names.length === 1 &&
      entry.tool_call_names[0] === 'user_tell';
  }

  function isHiddenUserTellResult(entry) {
    return entry.type === 'tool_result' && entry.tool_name === 'user_tell';
  }

  function buildHistoricalAssistantElement(entry) {
    const div = buildMessageElement(
      'assistant',
      entry.assistant_text_preview || '',
      modelCallMetaFor(entry),
      null
    );
    div.classList.add('historical-assistant');
    div.dataset.modelCall = 'true';
    div.title = 'Click to load full response';
    div.tabIndex = 0;
    div.setAttribute('role', 'button');
    div.addEventListener('click', function () {
      if (div.classList.contains('detail-loaded')) return;
      requestTranscriptDetail(entry.seq);
    });
    div.addEventListener('keydown', function (event) {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        if (div.classList.contains('detail-loaded')) return;
        requestTranscriptDetail(entry.seq);
      }
    });
    return div;
  }

  function buildTraceElement(entry) {
    const row = document.createElement('div');
    row.className = 'trace ' + entry.type;
    row.tabIndex = 0;
    row.setAttribute('role', 'button');

    const title = document.createElement('div');
    title.className = 'trace-title';
    title.textContent = titleFor(entry);
    row.appendChild(title);

    const summary = document.createElement('div');
    summary.className = 'trace-summary';
    summary.textContent = summaryFor(entry);
    row.appendChild(summary);

    const detail = document.createElement('div');
    detail.className = 'trace-detail';
    detail.hidden = true;
    detail.addEventListener('click', function (event) {
      event.stopPropagation();
    });
    row.appendChild(detail);

    row.addEventListener('click', function () {
      if (!detail.hidden) {
        detail.hidden = true;
        return;
      }
      requestTranscriptDetail(entry.seq);
    });
    row.addEventListener('keydown', function (event) {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        requestTranscriptDetail(entry.seq);
      }
    });
    return row;
  }

  function insertTranscriptElement(element, mode) {
    if (mode === 'prepend' && loadMoreEl && loadMoreEl.nextSibling) {
      messagesEl.insertBefore(element, loadMoreEl.nextSibling);
    } else if (mode === 'prepend' && loadMoreEl) {
      messagesEl.appendChild(element);
    } else {
      messagesEl.appendChild(element);
    }
  }

  function metaFor(entry) {
    return '#' + entry.seq + ' ' + (entry.ts || '');
  }

  function modelCallMetaFor(entry) {
    const tokens = entry.total_tokens ? entry.total_tokens + ' tokens' : 'tokens unknown';
    return metaFor(entry) + ' - API round ' + (entry.round || 0) + ' - ' + tokens;
  }

  function titleFor(entry) {
    if (entry.type === 'model_call') return 'API call round ' + (entry.round || 0);
    if (entry.type === 'tool_result') return 'Tool response: ' + (entry.tool_name || 'unknown');
    if (entry.type === 'compaction') return 'Compaction';
    return entry.type;
  }

  function summaryFor(entry) {
    if (entry.type === 'model_call') {
      const tokens = entry.total_tokens ? entry.total_tokens + ' tokens' : 'tokens unknown';
      const tools = entry.tool_call_names && entry.tool_call_names.length
        ? 'tools: ' + entry.tool_call_names.join(', ')
        : 'no tool calls';
      const preview = entry.assistant_text_preview ? ' - ' + entry.assistant_text_preview : '';
      return tokens + '; ' + tools + preview;
    }
    if (entry.type === 'tool_result') {
      const status = entry.errored ? 'failed' : 'ok';
      return status + '; ' + (entry.output_len || 0) + ' bytes';
    }
    if (entry.type === 'compaction') {
      return (entry.tokens_before || 0) + ' -> ' + (entry.tokens_after || 0) + ' tokens';
    }
    return entry.text || '';
  }

  function requestTranscriptDetail(seq) {
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      appendEvent('WebSocket is not connected; detail is unavailable.');
      return;
    }
    const row = messagesEl.querySelector('[data-seq="' + seq + '"]');
    if (row) row.classList.add('loading');
    ws.send(JSON.stringify({
      type: 'transcript_detail',
      request_id: 'seq-' + seq + '-' + Date.now(),
      conversation_key: currentConversation,
      seq_start: seq,
      seq_end: seq + 1,
    }));
  }

  function renderTranscriptDetail(entries) {
    entries.forEach(function (entry) {
      const row = messagesEl.querySelector('[data-seq="' + entry.seq + '"]');
      if (!row) return;
      row.classList.remove('loading');
      if (row.dataset.modelCall === 'true') {
        const body = row.querySelector('.message-body');
        if (body) {
          renderMessageBody(body, 'assistant', assistantTextFromEntry(entry));
          row.classList.add('detail-loaded');
          row.removeAttribute('title');
        }
        return;
      }
      const detail = row.querySelector('.trace-detail');
      if (!detail) return;
      detail.textContent = '';
      detail.hidden = false;
      detail.appendChild(detailContent(entry));
    });
  }

  function assistantTextFromEntry(entry) {
    const message = entry.assistant_message || {};
    const content = message.content;
    if (typeof content === 'string') return content;
    if (Array.isArray(content)) {
      return content.map(function (part) {
        if (typeof part === 'string') return part;
        if (part && typeof part.text === 'string') return part.text;
        return '';
      }).filter(Boolean).join('\n\n');
    }
    if (entry.assistant_text_preview) return entry.assistant_text_preview;
    return '';
  }

  function detailContent(entry) {
    const wrap = document.createElement('div');
    if (entry.type === 'model_call') {
      appendDetailLine(wrap, 'prompt_tokens', entry.prompt_tokens);
      appendDetailLine(wrap, 'completion_tokens', entry.completion_tokens);
      appendDetailLine(wrap, 'total_tokens', entry.total_tokens);
      appendPre(wrap, 'assistant_message', entry.assistant_message || {});
    } else if (entry.type === 'tool_result') {
      appendDetailLine(wrap, 'tool_call_id', entry.tool_call_id);
      appendDetailLine(wrap, 'tool_name', entry.tool_name);
      appendDetailLine(wrap, 'errored', entry.errored);
      appendPre(wrap, 'output', entry.output || '');
    } else {
      appendPre(wrap, 'entry', entry);
    }
    return wrap;
  }

  function appendDetailLine(parent, label, value) {
    const line = document.createElement('div');
    line.className = 'detail-line';
    line.textContent = label + ': ' + (value == null ? '' : String(value));
    parent.appendChild(line);
  }

  function appendPre(parent, label, value) {
    const caption = document.createElement('div');
    caption.className = 'detail-label';
    caption.textContent = label;
    parent.appendChild(caption);

    const pre = document.createElement('pre');
    pre.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
    parent.appendChild(pre);
  }

  function scrollToBottom() {
    autoStickToBottom = true;
    const apply = function () {
      messagesEl.scrollTop = messagesEl.scrollHeight;
    };
    apply();
    window.requestAnimationFrame(function () {
      apply();
      window.requestAnimationFrame(apply);
    });
    window.setTimeout(apply, 80);
    window.setTimeout(apply, 250);
    window.setTimeout(apply, 700);
  }

  function isNearBottom(padding) {
    const remaining = messagesEl.scrollHeight - messagesEl.clientHeight - messagesEl.scrollTop;
    return remaining <= (padding || 0);
  }

  function nearestNestedScrollable(target) {
    let node = target instanceof Element ? target : null;
    while (node && node !== messagesEl) {
      const style = window.getComputedStyle(node);
      const overflowY = style.overflowY;
      if (
        (overflowY === 'auto' || overflowY === 'scroll') &&
        node.scrollHeight > node.clientHeight + 1
      ) {
        return node;
      }
      node = node.parentElement;
    }
    return null;
  }

  function canScrollForWheel(element, deltaY) {
    if (!element || deltaY === 0) return false;
    if (deltaY < 0) return element.scrollTop > 0;
    const max = element.scrollHeight - element.clientHeight;
    return element.scrollTop < max - 1;
  }

  function connectWS() {
    if (reconnectTimer) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    ws = new window.WebSocket(wsUrl());

    ws.onopen = function () {
      statusEl.className = 'connected';
      statusEl.title = 'Connected';
      if (token) ws.send(JSON.stringify({ type: 'auth', token: token }));
    };
    ws.onclose = function () {
      statusEl.className = 'disconnected';
      statusEl.title = 'Disconnected';
      reconnectTimer = window.setTimeout(connectWS, 3000);
    };
    ws.onerror = function () {
      ws.close();
    };
    ws.onmessage = function (evt) {
      try {
        handleEvent(JSON.parse(evt.data));
      } catch (e) {
        window.console.warn('bad ws message', evt.data);
      }
    };
  }

  function reconnectWS() {
    if (ws) {
      ws.onclose = null;
      ws.close();
    }
    connectWS();
  }

  function handleEvent(data) {
    if (data.conversation_key && data.conversation_key !== currentConversation) return;

    switch (data.type) {
      case 'outgoing_message':
        appendMessage(
          'assistant',
          data.text,
          null,
          data.options,
          (data.images || []).concat(data.attachments || [])
        );
        break;
      case 'transcript_append':
        if (isUserTellModelCall(data.entry) || isHiddenUserTellResult(data.entry)) break;
        if (isAssistantModelCall(data.entry)) break;
        {
          const shouldStick = autoStickToBottom || isNearBottom(160);
          renderSkeleton(data.entry, 'append');
          if (shouldStick) scrollToBottom();
        }
        break;
      case 'transcript_detail':
        renderTranscriptDetail(data.entries || []);
        break;
      case 'transcript_error':
        appendEvent('Transcript detail failed: ' + data.message);
        break;
      case 'processing':
        if (data.state === 'typing') appendEvent('Processing...');
        break;
      case 'progress':
        if (data.text) appendEvent(data.text);
        break;
      case 'media_group':
        appendEvent('Media group (' + data.count + ' items)');
        break;
      default:
        break;
    }
  }

  formEl.addEventListener('submit', function (e) {
    e.preventDefault();
    const text = inputEl.value.trim();
    if (!text) return;
    inputEl.value = '';
    sendMessage(text);
  });

  inputEl.addEventListener('keydown', function (e) {
    if (e.isComposing || composingInput || e.keyCode === 229) return;
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      formEl.dispatchEvent(new Event('submit'));
    }
  });

  inputEl.addEventListener('compositionstart', function () {
    composingInput = true;
  });

  inputEl.addEventListener('compositionend', function () {
    composingInput = false;
  });

  messagesEl.addEventListener('scroll', function () {
    autoStickToBottom = isNearBottom(160);
    if (messagesEl.scrollTop < 20) {
      loadTranscriptPage('older');
    }
  });

  mainEl.addEventListener('wheel', function (event) {
    if (event.deltaY === 0) return;
    if (event.target instanceof Element && event.target.closest('#input')) return;
    const nested = nearestNestedScrollable(event.target);
    if (nested && canScrollForWheel(nested, event.deltaY)) return;
    if (messagesEl.scrollHeight <= messagesEl.clientHeight + 1) return;
    const max = Math.max(0, messagesEl.scrollHeight - messagesEl.clientHeight);
    const next = Math.max(0, Math.min(max, messagesEl.scrollTop + event.deltaY));
    if (next === messagesEl.scrollTop) return;
    event.preventDefault();
    messagesEl.scrollTop = next;
    autoStickToBottom = isNearBottom(160);
    if (messagesEl.scrollTop < 20) {
      loadTranscriptPage('older');
    }
  }, { passive: false });

  newConversationBtn.addEventListener('click', createConversation);
  refreshEl.addEventListener('click', function () {
    loadConversations().then(loadConversationState).then(function () {
      resetTranscript();
      return loadTranscriptPage('latest');
    }).catch(function (error) {
      appendEvent('Refresh failed: ' + error.message);
    });
  });
  applyWorkspaceBtn.addEventListener('click', function () {
    bindCurrentConversation(Boolean(currentConversationSummary))
      .then(loadConversations)
      .then(loadConversationState)
      .catch(function (error) {
        appendEvent('Workspace update failed: ' + error.message);
        openSettings();
      });
  });
  settingsBtn.addEventListener('click', openSettings);
  sidebarSettingsBtn.addEventListener('click', openSettings);
  heroConfigureBtn.addEventListener('click', openSettings);
  heroRefreshBtn.addEventListener('click', function () {
    loadTranscriptPage('latest');
  });
  closeSettingsBtn.addEventListener('click', closeSettings);
  settingsBackdropEl.addEventListener('click', closeSettings);
  workspaceKindInputEl.addEventListener('change', updateWorkspaceFieldVisibility);
  serverUrlInputEl.addEventListener('input', persistServerSettingsDraft);
  authTokenInputEl.addEventListener('input', persistServerSettingsDraft);
  saveWorkspaceDraftBtn.addEventListener('click', function () {
    saveWorkspaceDraft();
    updateCurrentConversationCard();
  });
  saveServerSettingsBtn.addEventListener('click', function () {
    saveServerSettings();
    closeSettings();
    reconnectWS();
    loadConversations().then(loadConversationState).catch(function (error) {
      appendEvent('Server reconnect failed: ' + error.message);
    });
  });
  bindWorkspaceBtn.addEventListener('click', function () {
    bindCurrentConversation(Boolean(currentConversationSummary))
      .then(function () {
        closeSettings();
        return loadConversations();
      })
      .then(loadConversationState)
      .then(function () {
        resetTranscript();
        return loadTranscriptPage('latest');
      })
      .catch(function (error) {
        appendEvent('Workspace update failed: ' + error.message);
      });
  });

  async function bootstrap() {
    serverUrlInputEl.value = serverBase;
    authTokenInputEl.value = token;
    loadWorkspaceDraft();
    updateServerPill();
    updateWorkspacePill();
    if (!currentConversation) {
      currentConversation = generateConversationKey();
    }
    setConversation(currentConversation);
    resetTranscript();
    try {
      await loadConversations();
      await loadConversationState();
      await loadTranscriptPage('latest');
    } catch (error) {
      appendEvent('Bootstrap failed: ' + error.message);
      openSettings();
    }
    connectWS();
    updateHeroState();
  }

  bootstrap();
})();
