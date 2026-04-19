// ClawParty Web client
(function () {
  'use strict';

  const AUTH_KEY = 'clawparty_auth_token';
  const CONVERSATION_KEY = 'clawparty_conversation_key';
  const PAGE_SIZE = 30;

  const messagesEl = document.getElementById('messages');
  const inputEl = document.getElementById('input');
  const formEl = document.getElementById('input-form');
  const statusEl = document.getElementById('connection-status');
  const conversationListEl = document.getElementById('conversation-list');
  const conversationTitleEl = document.getElementById('conversation-title');
  const newConversationBtn = document.getElementById('new-conversation-btn');
  const refreshEl = document.getElementById('refresh-btn');

  let ws = null;
  let token = localStorage.getItem(AUTH_KEY) || '';
  let currentConversation = (
    new URLSearchParams(location.search).get('conversation') ||
    localStorage.getItem(CONVERSATION_KEY) ||
    'web-default'
  );
  let transcriptOffset = 0;
  let hasMoreTranscript = true;
  let loadMoreEl = null;
  let loadingOlder = false;
  let conversationCache = [];
  let autoStickToBottom = true;
  const renderedSeqs = new Set();

  function ensureToken(force) {
    if (force) {
      token = '';
      localStorage.removeItem(AUTH_KEY);
    }
    if (!token) {
      token = prompt('Enter auth token (or leave empty for open channels):') || '';
      if (token) localStorage.setItem(AUTH_KEY, token);
    }
    return token;
  }

  function authHeaders() {
    const h = { 'Content-Type': 'application/json' };
    if (token) h.Authorization = 'Bearer ' + token;
    return h;
  }

  async function apiGet(path, retried) {
    const resp = await fetch(path, { headers: authHeaders() });
    if (resp.status === 401 && !retried) {
      ensureToken(true);
      reconnectWS();
      return apiGet(path, true);
    }
    if (!resp.ok) throw new Error(resp.status + ' ' + (await resp.text()));
    return resp.json();
  }

  async function apiPost(path, body, retried) {
    const resp = await fetch(path, {
      method: 'POST',
      headers: authHeaders(),
      body: JSON.stringify(body),
    });
    if (resp.status === 401 && !retried) {
      ensureToken(true);
      reconnectWS();
      return apiPost(path, body, true);
    }
    return resp;
  }

  async function apiDelete(path, body, retried) {
    const resp = await fetch(path, {
      method: 'DELETE',
      headers: authHeaders(),
      body: JSON.stringify(body),
    });
    if (resp.status === 401 && !retried) {
      ensureToken(true);
      reconnectWS();
      return apiDelete(path, body, true);
    }
    return resp;
  }

  function setConversation(key) {
    currentConversation = key || 'web-default';
    localStorage.setItem(CONVERSATION_KEY, currentConversation);
    const url = new URL(location.href);
    url.searchParams.set('conversation', currentConversation);
    history.replaceState(null, '', url);
    updateConversationTitle();
    renderConversationList(conversationCache);
  }

  async function loadConversations() {
    if (!conversationListEl) return;
    try {
      conversationCache = await apiGet('/api/conversations');
      if (!conversationCache.some(function (item) { return item.conversation_key === currentConversation; })) {
        conversationCache.unshift({
          conversation_key: currentConversation,
          entry_count: 0,
          latest_summary: 'No messages yet',
        });
      }
      renderConversationList(conversationCache);
    } catch (e) {
      appendEvent('Conversation list failed: ' + e.message);
    }
  }

  function renderConversationList(conversations) {
    if (!conversationListEl) return;
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

      const summary = document.createElement('div');
      summary.className = 'conversation-summary';
      summary.textContent = conversation.latest_summary || 'No messages yet';
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
    updateConversationTitle();
  }

  function updateConversationTitle() {
    if (conversationTitleEl) conversationTitleEl.textContent = currentConversation;
  }

  function switchConversation(key) {
    if (!key || key === currentConversation) return;
    setConversation(key);
    resetTranscript();
    loadTranscriptPage('latest');
  }

  async function createConversation() {
    try {
      const resp = await apiPost('/api/conversation', {});
      if (!resp.ok) {
        appendEvent('Create conversation failed: ' + resp.status + ' ' + (await resp.text()));
        return;
      }
      const conversation = await resp.json();
      conversationCache = [conversation].concat(
        conversationCache.filter(function (item) {
          return item.conversation_key !== conversation.conversation_key;
        })
      );
      setConversation(conversation.conversation_key);
      resetTranscript();
      renderConversationList(conversationCache);
      await loadConversations();
      await loadTranscriptPage('latest');
      inputEl.focus();
    } catch (e) {
      appendEvent('Create conversation failed: ' + e.message);
    }
  }

  async function deleteConversation(key) {
    if (!key) return;
    if (!confirm('Delete conversation "' + key + '"?')) return;
    try {
      const resp = await apiDelete('/api/conversation', { conversation_key: key });
      if (!resp.ok) {
        appendEvent('Delete conversation failed: ' + resp.status + ' ' + (await resp.text()));
        return;
      }
      const wasCurrent = key === currentConversation;
      conversationCache = conversationCache.filter(function (item) {
        return item.conversation_key !== key;
      });
      await loadConversations();
      if (!wasCurrent) {
        renderConversationList(conversationCache);
        return;
      }
      const next = conversationCache.find(function (item) {
        return item.conversation_key !== key;
      });
      if (next) {
        setConversation(next.conversation_key);
        resetTranscript();
        await loadTranscriptPage('latest');
      } else {
        resetTranscript();
        await createConversation();
      }
    } catch (e) {
      appendEvent('Delete conversation failed: ' + e.message);
    }
  }

  function appendMessage(role, text, meta, options, attachments) {
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
    scrollToBottom();
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
      while (index < lines.length && lines[index].trim() && !isMarkdownBlockStart(lines[index])) {
        paragraphLines.push(lines[index]);
        index += 1;
      }
      const p = document.createElement('p');
      renderInlineMarkdown(p, paragraphLines.join(' '));
      parent.appendChild(p);
    }
  }

  function isMarkdownBlockStart(line) {
    return /^```/.test(line) ||
      /^(#{1,6})\s+/.test(line) ||
      /^>\s?/.test(line) ||
      /^(\s*)([-*+]|\d+\.)\s+/.test(line);
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
      const url = new URL(raw, location.href);
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
    const params = new URLSearchParams({
      conversation_key: currentConversation,
      source: source,
      path: path,
    });
    if (token) params.set('token', token);
    return '/api/attachment?' + params.toString();
  }

  function guessAttachmentKind(path) {
    return /\.(png|jpe?g|gif|webp|avif|apng|svg)$/i.test(path) ? 'Image' : 'File';
  }

  function isImageAttachment(kind, path) {
    return String(kind).toLowerCase() === 'image' || guessAttachmentKind(path) === 'Image';
  }

  async function sendMessage(text) {
    appendMessage('user', text);
    try {
      const resp = await apiPost('/api/send', {
        text: text,
        conversation_key: currentConversation,
      });
      if (!resp.ok) {
        appendEvent('Send failed: ' + resp.status + ' ' + (await resp.text()));
      } else {
        loadConversations();
      }
    } catch (e) {
      appendEvent('Network error: ' + e.message);
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
  }

  async function loadTranscriptPage(mode) {
    if (mode === 'older' && !hasMoreTranscript) return;
    if (mode === 'older' && loadingOlder) return;
    if (mode === 'older') loadingOlder = true;
    const previousHeight = messagesEl.scrollHeight;
    const previousTop = messagesEl.scrollTop;
    const offset = mode === 'older' ? transcriptOffset : 0;
    const params = new URLSearchParams({
      conversation_key: currentConversation,
      offset: String(offset),
      limit: String(PAGE_SIZE),
    });
    try {
      const newestFirst = await apiGet('/api/transcript?' + params.toString());
      const chronological = newestFirst.slice().reverse();
      const entriesToRender = mode === 'older' ? chronological.slice().reverse() : chronological;
      entriesToRender.forEach(function (entry) {
        renderSkeleton(entry, mode === 'older' ? 'prepend' : 'append');
      });
      if (mode === 'older') transcriptOffset += newestFirst.length;
      if (mode !== 'older') transcriptOffset = Math.max(transcriptOffset, newestFirst.length);
      hasMoreTranscript = newestFirst.length === PAGE_SIZE;
      updateLoadMore();
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
    requestAnimationFrame(function () {
      apply();
      requestAnimationFrame(apply);
    });
    setTimeout(apply, 80);
    setTimeout(apply, 250);
    setTimeout(apply, 700);
  }

  function isNearBottom(padding) {
    const remaining = messagesEl.scrollHeight - messagesEl.clientHeight - messagesEl.scrollTop;
    return remaining <= (padding || 0);
  }

  function connectWS() {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    ws = new WebSocket(proto + '//' + location.host + '/ws');

    ws.onopen = function () {
      statusEl.className = 'connected';
      statusEl.title = 'Connected';
      if (token) ws.send(JSON.stringify({ type: 'auth', token: token }));
    };
    ws.onclose = function () {
      statusEl.className = 'disconnected';
      statusEl.title = 'Disconnected';
      setTimeout(connectWS, 3000);
    };
    ws.onerror = function () {
      ws.close();
    };
    ws.onmessage = function (evt) {
      try {
        handleEvent(JSON.parse(evt.data));
      } catch (e) {
        console.warn('bad ws message', evt.data);
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
        renderSkeleton(data.entry, 'append');
        scrollToBottom();
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
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      formEl.dispatchEvent(new Event('submit'));
    }
  });

  if (newConversationBtn) {
    newConversationBtn.addEventListener('click', function () {
      createConversation();
    });
  }

  if (refreshEl) {
    refreshEl.addEventListener('click', function () {
      loadTranscriptPage('latest');
    });
  }

  messagesEl.addEventListener('scroll', function () {
    if (!loadingOlder) {
      autoStickToBottom = isNearBottom(160);
    }
    if (messagesEl.scrollTop <= 24) {
      loadTranscriptPage('older');
    }
  });

  ensureToken();
  setConversation(currentConversation);
  resetTranscript();
  loadConversations().then(function () {
    return loadTranscriptPage('latest');
  });
  connectWS();
})();
