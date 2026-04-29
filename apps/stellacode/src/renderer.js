const $ = (selector) => document.querySelector(selector);
const INITIAL_MESSAGE_LIMIT = 40;
const MESSAGE_PAGE_LIMIT = 40;

const state = {
  settings: null,
  activeServerId: null,
  selected: null,
  conversations: new Map(),
  statuses: new Map(),
  serverHealth: new Map(),
  workspaceListings: new Map(),
  workspacePaths: new Map(),
  workspaceExpandedPaths: new Map(),
  workspaceErrors: new Map(),
  workspaceLoading: new Set(),
  workspaceFilePreviews: new Map(),
  workspaceFileErrors: new Map(),
  workspaceFileLoading: new Set(),
  fileTabs: [],
  fileViewModes: new Map(),
  workspaceFilter: '',
  terminals: new Map(),
  terminalOutput: new Map(),
  terminalOffsets: new Map(),
  terminalInputQueues: new Map(),
  xtermSessions: new Map(),
  activeTerminalId: null,
  terminalPoll: null,
  messages: [],
  messagePageStart: 0,
  messagePageTotal: 0,
  loadingOlderMessages: false,
  optimisticMessages: [],
  messagesSignature: '',
  messageDetails: new Map(),
  messageDetailsLoading: new Set(),
  expandedMessages: new Set(),
  expandedExecutionGroups: new Set(),
  activeContextTab: 'overview',
  activePreviewMessageId: null,
  activePreviewFilePath: null,
  contextCollapsed: true,
  fileBarOpen: false,
  terminalOpen: false,
  layout: {
    sidebar: 286,
    context: 340,
    file: 360,
    terminal: 240
  },
  activePoll: null,
  websocket: null,
  websocketKey: '',
  websocketReconnectTimer: null,
  sessionActivity: '',
  sessionActivityClearTimer: null,
  sessionProgress: null,
  channelEvents: [],
  metadataPopover: null,
  tokenUsagePopover: null,
  conversationMenu: null,
  refreshEpoch: 0,
  isRefreshing: false,
  lastRefreshAt: null,
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
  toggleFileButton: $('#toggleFileButton'),
  toggleTerminalButton: $('#toggleTerminalButton'),
  toggleSidebarButton: $('#toggleSidebarButton'),
  newConversationButton: $('#newConversationButton'),
  serverStatusButton: $('#serverStatusButton'),
  serverPopover: $('#serverPopover'),
  settingsButton: $('#settingsButton'),
  contextContent: $('#contextContent'),
  fileContent: $('#fileContent'),
  terminalContent: $('#terminalContent'),
  collapseContextButton: $('#collapseContextButton'),
  modalLayer: $('#modalLayer')
};

const layoutStorageKey = 'stellacode.layout.v1';

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

function normalizeTerminalOutput(value) {
  return safeText(value)
    .replace(/\u001b\][^\u0007]*(?:\u0007|\u001b\\)/g, '')
    .replace(/\u001b\[[0-?]*[ -/]*[@-~]/g, '')
    .replace(/\u001b[()][A-Za-z0-9]/g, '')
    .replace(/\u001b[=>]/g, '')
    .replace(/\u000f|\u000e|\u001b/g, '')
    .replace(/\r\n/g, '\n')
    .replace(/\r/g, '\n');
}

function terminalDisplayOutput(value) {
  const lines = safeText(value).replace(/[ \t]+$/gm, '').split('\n');
  while (lines.length > 0 && lines.at(-1).trim() === '') {
    lines.pop();
  }
  if (lines.length > 0 && /^[#$%]\s*$/.test(lines.at(-1).trim())) {
    lines.pop();
    while (lines.length > 0 && lines.at(-1).trim() === '') {
      lines.pop();
    }
    if (
      lines.length > 0 &&
      (/^[╭╰┌└]/u.test(lines.at(-1).trim()) ||
        (lines.at(-1).includes(' on ') && lines.at(-1).includes(' at ')))
    ) {
      lines.pop();
    }
  }
  return lines.join('\n');
}

const icons = {
  search:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M10.8 18.1a7.3 7.3 0 1 1 0-14.6 7.3 7.3 0 0 1 0 14.6Z"/><path d="m16.1 16.1 4.4 4.4"/></svg>',
  refresh:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20 12a8 8 0 1 1-2.34-5.66"/><path d="M20 4v6h-6"/></svg>',
  chevronRight:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg>',
  chevronDown:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg>',
  folder:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3.5 7.5a2 2 0 0 1 2-2h4.2l2 2H18a2.5 2.5 0 0 1 2.5 2.5v6.5a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2Z"/></svg>',
  file:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6.5 3.5h7l4 4v13h-11Z"/><path d="M13.5 3.5v4h4"/></svg>',
  symlink:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 7H7a4 4 0 0 0 0 8h2"/><path d="M15 7h2a4 4 0 0 1 0 8h-2"/><path d="M8 12h8"/></svg>',
  terminal:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5.5h16v13H4Z"/><path d="m8 9 3 3-3 3"/><path d="M13 15h4"/></svg>',
  check:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12 4 4 10-10"/></svg>',
  panelOpen:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5.5h16v13H4Z"/><path d="M15 5.5v13"/><path d="m10 9 3 3-3 3"/></svg>',
  panelClose:
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5.5h16v13H4Z"/><path d="M15 5.5v13"/><path d="m13 9-3 3 3 3"/></svg>'
};

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

function formatElapsed(startValue, endValue) {
  const start = new Date(startValue || '').getTime();
  const end = new Date(endValue || '').getTime();
  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) {
    return '';
  }
  const seconds = Math.max(1, Math.round((end - start) / 1000));
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  if (minutes < 60) {
    return rest ? `${minutes}m ${rest}s` : `${minutes}m`;
  }
  const hours = Math.floor(minutes / 60);
  const minuteRest = minutes % 60;
  return minuteRest ? `${hours}h ${minuteRest}m` : `${hours}h`;
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

function markdownToHtml(value) {
  const text = safeText(value);
  if (!text) {
    return '';
  }
  const markedApi = window.marked?.parse ? window.marked : window.marked?.marked ? window.marked.marked : null;
  if (!markedApi?.parse || !window.DOMPurify?.sanitize) {
    return renderMarkdownLines(text);
  }
  const rendered = markedApi.parse(text, {
    async: false,
    breaks: true,
    gfm: true,
    mangle: false,
    headerIds: false
  });
  return window.DOMPurify.sanitize(rendered, {
    ALLOWED_TAGS: [
      'a',
      'blockquote',
      'br',
      'code',
      'del',
      'em',
      'h2',
      'h3',
      'h4',
      'h5',
      'h6',
      'hr',
      'img',
      'li',
      'ol',
      'p',
      'pre',
      'span',
      'strong',
      'table',
      'tbody',
      'td',
      'th',
      'thead',
      'tr',
      'ul'
    ],
    ALLOWED_ATTR: ['alt', 'class', 'href', 'src', 'title'],
    ALLOW_DATA_ATTR: false
  });
}

function formatBytes(value) {
  const bytes = Number(value || 0);
  if (bytes <= 0) {
    return '';
  }
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let size = bytes;
  let index = 0;
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }
  return `${size >= 10 || index === 0 ? size.toFixed(0) : size.toFixed(1)} ${units[index]}`;
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
      text: message.text,
      text_with_attachment_markers: message.text_with_attachment_markers,
      preview: message.preview,
      items: message.items,
      attachments: message.attachments,
      attachment_count: message.attachment_count,
      has_attachment_errors: message.has_attachment_errors,
      has_token_usage: message.has_token_usage,
      token_usage: message.token_usage
    }))
  );
}

function clearActivePoll() {
  if (state.activePoll) {
    clearTimeout(state.activePoll);
    state.activePoll = null;
  }
}

function clearWebsocketSubscription() {
  if (state.websocketReconnectTimer) {
    clearTimeout(state.websocketReconnectTimer);
    state.websocketReconnectTimer = null;
  }
  if (state.sessionActivityClearTimer) {
    clearTimeout(state.sessionActivityClearTimer);
    state.sessionActivityClearTimer = null;
  }
  state.websocketKey = '';
  const socket = state.websocket;
  state.websocket = null;
  if (socket && socket.readyState <= WebSocket.OPEN) {
    socket.close();
  }
}

function clearTerminalPoll() {
  if (state.terminalPoll) {
    clearTimeout(state.terminalPoll);
    state.terminalPoll = null;
  }
}

function visibleMessages() {
  const key = selectedKey();
  if (!key) {
    return attachRuntimeMetadataToUserMessages(state.messages);
  }
  return attachRuntimeMetadataToUserMessages([
    ...state.messages,
    ...state.optimisticMessages.filter((message) => message.conversationKey === key)
  ]);
}

function isSyntheticMediaContextMessage(message) {
  if (safeText(message?.role) !== 'user') {
    return false;
  }
  return safeText(message?.preview).trim() === 'Tool returned media files. Use the attached files as current context.';
}

function firstTextItem(message) {
  const item = messageItems(message).find((entry) => entry.type === 'text');
  return safeText(item?.text || message?.text || message?.preview).trim();
}

function runtimeMetadataInfo(message) {
  if (roleClass(message?.role) !== 'user' || messageHasItemType(message, ['file', 'tool_call', 'tool_result'])) {
    return null;
  }
  const text = firstTextItem(message);
  const markers = [
    {
      prefix: '[Incoming User Metadata]',
      kind: 'incoming',
      label: '用户元数据',
      className: 'incoming'
    },
    {
      prefix: '[Runtime Prompt Updates]',
      kind: 'prompt',
      label: 'Prompt 更新',
      className: 'prompt'
    },
    {
      prefix: '[Runtime Skill Updates]',
      kind: 'skill',
      label: 'Skill 更新',
      className: 'skill'
    },
    {
      prefix: '[Legacy system message]',
      kind: 'legacy',
      label: 'Legacy System',
      className: 'legacy'
    }
  ];
  const marker = markers.find((item) => text.startsWith(item.prefix));
  if (!marker) {
    return null;
  }
  return {
    ...marker,
    text,
    messageId: safeText(message.id)
  };
}

function attachRuntimeMetadataToUserMessages(messages) {
  const result = [];
  let pending = [];
  for (const message of messages) {
    const metadata = runtimeMetadataInfo(message);
    if (metadata) {
      pending.push(metadata);
      continue;
    }
    if (pending.length > 0 && roleClass(message?.role) === 'user') {
      result.push({
        ...message,
        runtimeMetadata: [...(message.runtimeMetadata || []), ...pending]
      });
      pending = [];
      continue;
    }
    result.push(message);
  }
  return result;
}

function messageItems(message) {
  return Array.isArray(message?.items) ? message.items : [];
}

function messageHasItemType(message, types) {
  const wanted = new Set(types);
  return messageItems(message).some((item) => wanted.has(item.type));
}

function isExecutionMessage(message) {
  return messageHasItemType(message, ['tool_call', 'tool_result']) || isSyntheticMediaContextMessage(message);
}

function isFinalAssistantMessage(message) {
  if (roleClass(message?.role) !== 'assistant' || isExecutionMessage(message)) {
    return false;
  }
  return (
    messageItems(message).some((item) => item.type === 'text' && safeText(item.text).trim()) ||
    safeText(message?.preview || message?.text).trim()
  );
}

function removeOptimisticMessage(id) {
  state.optimisticMessages = state.optimisticMessages.filter((message) => message.id !== id);
}

function markOptimisticMessageSent(id) {
  const message = state.optimisticMessages.find((item) => item.id === id);
  if (!message) {
    return;
  }
  message.pending = false;
  message.localOnly = true;
}

function selectedConnectionMatches(serverId, conversationId) {
  return state.selected?.serverId === serverId && state.selected?.conversationId === conversationId;
}

function replaceOrAppendMessages(messages) {
  if (!Array.isArray(messages) || messages.length === 0) {
    return false;
  }
  const byId = new Map(state.messages.map((message) => [safeText(message.id), message]));
  let changed = false;
  for (const message of messages) {
    const id = safeText(message.id);
    if (stableSignature(byId.get(id)) !== stableSignature(message)) {
      byId.set(id, message);
      changed = true;
    }
  }
  if (!changed) {
    return false;
  }
  state.messages = Array.from(byId.values()).sort((left, right) => Number(left.index ?? left.id) - Number(right.index ?? right.id));
  state.messagePageStart = firstMessageIndex(state.messages);
  state.messagePageTotal = Math.max(state.messagePageTotal || 0, ...state.messages.map((message) => Number(message.index ?? message.id) + 1));
  state.messagesSignature = messageListSignature(state.messages);
  return true;
}

function renderMessagesPreservingViewport({ forceBottom = false } = {}) {
  const list = elements.messageList;
  const wasNearBottom = forceBottom || isMessageListNearBottom();
  const previousScrollTop = list.scrollTop;
  const previousScrollHeight = list.scrollHeight;
  renderMessages({ preserveScroll: true });
  if (wasNearBottom) {
    scrollMessagesToBottom();
    return;
  }
  list.scrollTop = previousScrollTop + (list.scrollHeight - previousScrollHeight);
}

function websocketUrl(baseUrl, token) {
  const url = new URL('/api/ws', baseUrl);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('token', token || '');
  return url.toString();
}

function setSessionActivity(text, options = {}) {
  if (state.sessionActivityClearTimer) {
    clearTimeout(state.sessionActivityClearTimer);
    state.sessionActivityClearTimer = null;
  }
  state.sessionActivity = safeText(text);
  renderHeader();
  if (options.clearAfterMs) {
    const expected = state.sessionActivity;
    state.sessionActivityClearTimer = setTimeout(() => {
      if (state.sessionActivity === expected) {
        state.sessionActivity = '';
        renderHeader();
      }
      state.sessionActivityClearTimer = null;
    }, options.clearAfterMs);
  }
}

function parseProgressFeedbackText(text) {
  const lines = safeText(text).split('\n').map((line) => line.trim()).filter(Boolean);
  const result = {
    title: lines[0]?.replace(/^[^\p{L}\p{N}]+/u, '') || '正在执行',
    model: '',
    phase: '',
    tip: '',
    plan: []
  };
  let inPlan = false;
  for (const line of lines.slice(1)) {
    if (line === '计划') {
      inPlan = true;
      continue;
    }
    if (inPlan) {
      const match = line.match(/^([☐◐☑])\s+(.+)$/u);
      if (match) {
        result.plan.push({ marker: match[1], text: match[2] });
      } else if (line) {
        result.plan.push({ marker: '', text: line });
      }
      continue;
    }
    if (line.includes('模型：')) {
      result.model = line.split('模型：').pop().trim();
    } else if (line.includes('阶段：')) {
      result.phase = line.split('阶段：').pop().trim();
    } else if (line.includes('状态：')) {
      result.phase = line.split('状态：').pop().trim();
    } else if (line.includes('发送新消息可打断')) {
      result.tip = line.replace(/^[^\p{L}\p{N}/]+/u, '').trim();
    }
  }
  return result;
}

function parseRunningToolPhase(phase) {
  const text = safeText(phase).trim();
  if (!text) {
    return null;
  }
  const batch = text.match(/^running tool batch\s+[^:]+:\s*(.+)$/i);
  const summary = (batch ? batch[1] : text).split(/\s*;\s*/).find(Boolean)?.trim() || '';
  const match = summary.match(/^([A-Za-z_][\w.-]*)(?:\s+([\s\S]+))?$/);
  if (!match) {
    return null;
  }
  const name = match[1];
  const hint = safeText(match[2]).trim();
  if (!hint) {
    return { name, payload: {} };
  }
  const payloadKey = name === 'shell' || name === 'exec' || name === 'exec_command' ? 'command' : 'input';
  return { name, payload: { [payloadKey]: hint } };
}

function openToolCallsFromMessages() {
  const open = new Map();
  for (const message of state.messages) {
    for (const item of messageItems(message)) {
      if (item.type === 'tool_call') {
        open.set(item.tool_call_id || `${item.tool_name}:${item.index}`, item);
      } else if (item.type === 'tool_result') {
        open.delete(item.tool_call_id || `${item.tool_name}:${item.index}`);
      }
    }
  }
  return [...open.values()];
}

function runningToolForProgress(parsed) {
  const openCalls = openToolCallsFromMessages();
  if (openCalls.length > 0) {
    const item = openCalls.at(-1);
    return {
      name: item.tool_name || 'tool',
      payload: item.arguments || {}
    };
  }
  return parseRunningToolPhase(parsed?.phase);
}

function displayProgressText(text) {
  const value = safeText(text).trim();
  if (!value) {
    return '正在处理';
  }
  const structured = parseProgressFeedbackText(value);
  const runningTool = runningToolForProgress(structured);
  if (runningTool) {
    return `正在调用 ${runningTool.name}`;
  }
  if (structured.phase) {
    return structured.phase;
  }
  const batch = value.match(/^running tool batch\s+[^:]+:\s*(.+)$/i);
  if (batch) {
    return `正在执行 ${batch[1]}`;
  }
  return structured.title || value.split('\n').find(Boolean) || '正在处理';
}

function handleProcessingEvent(payload) {
  if (payload.state === 'typing') {
    if (!state.sessionProgress || state.sessionProgress.conversationKey !== selectedKey()) {
      state.sessionProgress = {
        conversationKey: selectedKey(),
        turnId: '',
        important: false,
        rawText: '⚙️ 正在执行\n🧠 状态：思考中...',
        parsed: {
          title: '正在执行',
          model: selectedStatus()?.model || '',
          phase: '思考中...',
          tip: '',
          plan: []
        },
        updatedAt: Date.now()
      };
      renderMessagesPreservingViewport();
    }
    setSessionActivity('正在思考');
  } else if (payload.state === 'idle') {
    setSessionActivity('');
  }
}

function handleProgressFeedbackEvent(payload) {
  const text = displayProgressText(payload.text);
  if (payload.final_state === 'done') {
    if (state.sessionProgress?.conversationKey === selectedKey()) {
      state.sessionProgress = {
        ...state.sessionProgress,
        rawText: safeText(payload.text),
        parsed: {
          ...parseProgressFeedbackText(payload.text),
          phase: '执行完毕'
        },
        updatedAt: Date.now()
      };
      setTimeout(() => {
        if (state.sessionProgress?.parsed?.phase === '执行完毕') {
          state.sessionProgress = null;
          renderMessagesPreservingViewport();
        }
      }, 1600);
    }
    setSessionActivity('已完成', { clearAfterMs: 1400 });
    renderMessagesPreservingViewport();
    return;
  }
  if (payload.final_state === 'failed') {
    state.sessionProgress = {
      conversationKey: selectedKey(),
      turnId: safeText(payload.turn_id),
      important: Boolean(payload.important),
      rawText: safeText(payload.text),
      parsed: {
        ...parseProgressFeedbackText(payload.text),
        phase: '执行失败'
      },
      updatedAt: Date.now()
    };
    setSessionActivity('执行失败', { clearAfterMs: 2400 });
    renderMessagesPreservingViewport();
    return;
  }
  state.sessionProgress = {
    conversationKey: selectedKey(),
    turnId: safeText(payload.turn_id),
    important: Boolean(payload.important),
    rawText: safeText(payload.text),
    parsed: parseProgressFeedbackText(payload.text),
    updatedAt: Date.now()
  };
  setSessionActivity(text);
  renderMessagesPreservingViewport();
}

function handleStatusEvent(serverId, conversationId, payload) {
  if (payload.status && typeof payload.status === 'object') {
    const key = conversationKey(serverId, conversationId);
    const previous = state.statuses.get(key);
    const workspaceChanged =
      previous &&
      (safeText(previous.remote) !== safeText(payload.status.remote) ||
        safeText(previous.workspace) !== safeText(payload.status.workspace));
    state.statuses.set(key, payload.status);
    if (workspaceChanged && selectedConnectionMatches(serverId, conversationId)) {
      invalidateWorkspaceCache(key);
      refreshWorkspace().catch(() => {});
    }
    renderSidebar();
    renderHeader();
    renderContext();
  }
}

function normalizeChannelError(payload) {
  const error = payload?.error && typeof payload.error === 'object' ? payload.error : payload;
  return {
    id: createId('channel-error'),
    conversationKey: selectedKey(),
    createdAt: Date.now(),
    type: 'error',
    scope: safeText(error.scope || payload.scope || 'runtime'),
    severity: safeText(error.severity || payload.severity || 'error'),
    code: safeText(error.code || payload.code || 'error'),
    message: safeText(error.message || payload.message || payload.error || '未知错误'),
    detail: error.detail ?? payload.detail ?? null,
    canContinue: Boolean(error.can_continue ?? payload.can_continue),
    suggestedAction: safeText(error.suggested_action || payload.suggested_action || '')
  };
}

function addChannelEvent(event) {
  state.channelEvents = [...state.channelEvents, event]
    .filter((item) => item.conversationKey === selectedKey())
    .slice(-12);
}

function handleChannelErrorEvent(payload) {
  const event = normalizeChannelError(payload);
  addChannelEvent(event);
  const label = event.severity === 'warning' ? '警告' : event.severity === 'info' ? '提示' : '错误';
  setSessionActivity(`${label}: ${event.message}`, { clearAfterMs: event.severity === 'error' ? 4200 : 2600 });
  showToast(event.message);
  renderMessagesPreservingViewport();
}

function updateSessionActivityFromMessages(messages) {
  const last = [...messages].reverse().find((message) => Array.isArray(message.items) && message.items.length > 0);
  if (!last) {
    return;
  }
  const item = [...last.items].reverse().find((entry) => entry.type === 'tool_call' || entry.type === 'tool_result' || entry.type === 'text');
  if (!item) {
    return;
  }
  if (item.type === 'tool_call') {
    setSessionActivity(`正在调用 ${item.tool_name || '工具'}`);
  } else if (item.type === 'tool_result') {
    const context = safeText(item.context);
    let status = '';
    try {
      status = safeText(JSON.parse(context).status);
    } catch {
      status = '';
    }
    setSessionActivity(status === 'running'
      ? `${item.tool_name || '工具'} 运行中`
      : `${item.tool_name || '工具'} 已返回`);
  } else if (last.role === 'assistant') {
    setSessionActivity('正在回复');
  }
}

async function reconcileMessagesFromAck(serverId, conversationId, ack) {
  const nextId = safeText(ack.next_message_id);
  if (!nextId || !selectedConnectionMatches(serverId, conversationId)) {
    return;
  }
  const lastId = lastMessageId();
  if (!lastId) {
    const response = await fetchInitialConversationMessages(serverId, conversationId);
    if (!selectedConnectionMatches(serverId, conversationId)) {
      return;
    }
    state.messages = response.data?.messages || [];
    state.messagePageStart = Number(response.data?.offset || 0);
    state.messagePageTotal = Number(response.data?.total || state.messages.length);
    state.messagesSignature = messageListSignature(state.messages);
    renderMessages({ stickToBottom: true });
    return;
  }
  if (Number(nextId) > Number(lastId) + 1) {
    const response = await fetchConversationMessageRange(serverId, conversationId, lastId, {
      direction: 'after',
      includeAnchor: false,
      limit: Math.min(200, Number(nextId) - Number(lastId) - 1)
    });
    if (selectedConnectionMatches(serverId, conversationId) && replaceOrAppendMessages(response.data?.messages || [])) {
      renderMessagesPreservingViewport();
      updateSessionActivityFromMessages(response.data?.messages || []);
    }
  }
}

function handleWebsocketPayload(serverId, conversationId, payload) {
  if (!selectedConnectionMatches(serverId, conversationId)) {
    return;
  }
  if (payload.type === 'subscription_ack') {
    reconcileMessagesFromAck(serverId, conversationId, payload).catch(() => {});
    setSessionActivity(payload.reason === 'session_changed' ? 'Session 已切换' : '已连接实时消息', { clearAfterMs: 1200 });
    return;
  }
  if (payload.type === 'processing') {
    handleProcessingEvent(payload);
    return;
  }
  if (payload.type === 'progress_feedback') {
    handleProgressFeedbackEvent(payload);
    return;
  }
  if (payload.type === 'status') {
    handleStatusEvent(serverId, conversationId, payload);
    return;
  }
  if (payload.type === 'error') {
    handleChannelErrorEvent(payload);
    return;
  }
  if (payload.type === 'messages') {
    const messages = payload.messages || [];
    if (replaceOrAppendMessages(messages)) {
      state.optimisticMessages = state.optimisticMessages.filter((optimistic) => {
        if (optimistic.conversationKey !== conversationKey(serverId, conversationId)) {
          return true;
        }
        return !serverHasOptimisticMessage(messages, optimistic);
      });
      renderMessagesPreservingViewport();
      updateSessionActivityFromMessages(messages);
    }
  }
}

async function connectConversationWebsocket(serverId, conversationId) {
  clearWebsocketSubscription();
  const key = conversationKey(serverId, conversationId);
  state.websocketKey = key;
  try {
    const info = await window.stellacode.connectionInfo(serverId);
    if (state.websocketKey !== key) {
      return;
    }
    const socket = new WebSocket(websocketUrl(info.baseUrl, info.token));
    state.websocket = socket;
    socket.addEventListener('open', () => {
      socket.send(JSON.stringify({ type: 'subscribe_foreground', conversation_id: conversationId }));
    });
    socket.addEventListener('message', (event) => {
      try {
        handleWebsocketPayload(serverId, conversationId, JSON.parse(event.data));
      } catch (error) {
        console.warn('bad websocket payload', error);
      }
    });
    socket.addEventListener('close', () => {
      if (state.websocketKey !== key) {
        return;
      }
      state.websocketReconnectTimer = setTimeout(() => connectConversationWebsocket(serverId, conversationId), 2000);
    });
    socket.addEventListener('error', () => {
      setSessionActivity('实时连接异常，使用刷新兜底');
    });
  } catch (error) {
    if (state.websocketKey === key) {
      setSessionActivity('实时连接不可用，使用刷新兜底');
    }
  }
}

function serverHasOptimisticMessage(messages, optimistic) {
  const preview = safeText(optimistic.preview).trim();
  if (!preview) {
    return false;
  }
  return messages.some((message) => {
    if (String(message.role || '').toLowerCase() !== 'user') {
      return false;
    }
    return safeText(message.preview).trim() === preview;
  });
}

function lastMessageId(messages = state.messages) {
  return messages.length > 0 ? safeText(messages[messages.length - 1]?.id) : '';
}

function firstMessageIndex(messages = state.messages) {
  return messages.length > 0 ? Number(messages[0]?.index ?? messages[0]?.id ?? 0) : 0;
}

function messagePageHasOlder() {
  return state.messages.length > 0 && firstMessageIndex() > 0;
}

async function fetchConversationMessages(serverId, conversationId, options = {}) {
  const { incremental = true, offset = null, limit = MESSAGE_PAGE_LIMIT } = options;
  const lastId = incremental ? lastMessageId() : '';
  const path = lastId && offset === null
    ? `/api/conversations/${conversationId}/messages/after/${encodeURIComponent(lastId)}?limit=${encodeURIComponent(limit)}`
    : `/api/conversations/${conversationId}/messages?offset=${encodeURIComponent(offset ?? 0)}&limit=${encodeURIComponent(limit)}`;
  return api(serverId, path);
}

async function fetchConversationMessageRange(serverId, conversationId, anchorId, options = {}) {
  const direction = options.direction || 'after';
  const includeAnchor = options.includeAnchor ? 'true' : 'false';
  const limit = options.limit || MESSAGE_PAGE_LIMIT;
  return api(
    serverId,
    `/api/conversations/${conversationId}/messages/range?anchor_id=${encodeURIComponent(anchorId)}&direction=${encodeURIComponent(direction)}&include_anchor=${includeAnchor}&limit=${encodeURIComponent(limit)}`
  );
}

async function fetchInitialConversationMessages(serverId, conversationId) {
  const probe = await fetchConversationMessages(serverId, conversationId, {
    incremental: false,
    offset: 0,
    limit: 1
  });
  const total = Number(probe.data?.total || 0);
  const offset = Math.max(0, total - INITIAL_MESSAGE_LIMIT);
  if (total <= 1) {
    return probe;
  }
  return fetchConversationMessages(serverId, conversationId, {
    incremental: false,
    offset,
    limit: INITIAL_MESSAGE_LIMIT
  });
}

function setRefreshing(value) {
  state.isRefreshing = value;
  document.body.classList.toggle('is-refreshing', value);
  elements.refreshButton.disabled = value;
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

function visibleConversations(serverId) {
  return (state.conversations.get(serverId) || []).filter(
    (conversation) => !isConversationHidden(serverId, conversation.conversation_id)
  );
}

function selectedStatus() {
  if (!state.selected) {
    return null;
  }
  return state.statuses.get(conversationKey(state.selected.serverId, state.selected.conversationId));
}

function selectedKey() {
  if (!state.selected) {
    return '';
  }
  return conversationKey(state.selected.serverId, state.selected.conversationId);
}

function currentWorkspacePath() {
  const key = selectedKey();
  return key ? state.workspacePaths.get(key) || '' : '';
}

function workspaceCacheKey(key, path) {
  return `${key}::${normalizeWorkspacePath(path)}`;
}

function workspaceFileCacheKey(key, path) {
  return `${key}::file::${normalizeWorkspacePath(path)}`;
}

function invalidateWorkspaceCache(key) {
  const prefix = `${key}::`;
  for (const cacheKey of [...state.workspaceListings.keys()]) {
    if (cacheKey.startsWith(prefix)) {
      state.workspaceListings.delete(cacheKey);
    }
  }
  for (const cacheKey of [...state.workspaceErrors.keys()]) {
    if (cacheKey.startsWith(prefix)) {
      state.workspaceErrors.delete(cacheKey);
    }
  }
  for (const cacheKey of [...state.workspaceFilePreviews.keys()]) {
    if (cacheKey.startsWith(prefix)) {
      state.workspaceFilePreviews.delete(cacheKey);
    }
  }
  for (const cacheKey of [...state.workspaceFileErrors.keys()]) {
    if (cacheKey.startsWith(prefix)) {
      state.workspaceFileErrors.delete(cacheKey);
    }
  }
}

async function uploadFilesToWorkspace(files, targetDir) {
  if (!state.selected || files.length === 0) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  const key = selectedKey();
  const normalized = normalizeWorkspacePath(targetDir);
  setSessionActivity(`正在上传 ${files.length} 个文件...`);
  try {
    const tarModule = await import('https://cdn.jsdelivr.net/npm/tar-js@0.3.0/+esm').catch(() => null);
    // Use a simple approach: pack files into tar, gzip, and send.
    // Since we're in Electron, we can use a simpler method via IPC.
    const archiveData = await packFilesToTarGz(files);
    if (archiveData.byteLength > 10 * 1024 * 1024) {
      showToast('上传文件过大（压缩后超过 10MB 限制）');
      return;
    }
    await window.stellacode.uploadWorkspace({
      serverId,
      conversationId,
      path: normalized,
      data: Array.from(new Uint8Array(archiveData))
    });
    invalidateWorkspaceCache(key);
    await refreshWorkspace(normalized, { expand: true, setActive: true });
    setSessionActivity(`上传完成`, { clearAfterMs: 2000 });
  } catch (error) {
    showToast(`上传失败: ${error.message}`);
    setSessionActivity('上传失败', { clearAfterMs: 2000 });
  }
}

async function packFilesToTarGz(fileEntries) {
  // fileEntries: array of { relativePath: string, data: ArrayBuffer }
  // Build a minimal tar archive and gzip it.
  const blocks = [];
  for (const entry of fileEntries) {
    const nameBytes = new TextEncoder().encode(entry.relativePath);
    if (nameBytes.length > 99) {
      // Use extended header for long names (PAX)
      const paxContent = new TextEncoder().encode(`path=${entry.relativePath}\n`);
      const paxSize = paxContent.length;
      const paxHeader = createTarHeader('PaxHeader', paxSize, '0', 'x');
      blocks.push(paxHeader);
      blocks.push(padToBlock(paxContent));
    }
    const data = new Uint8Array(entry.data);
    const header = createTarHeader(
      entry.relativePath.length > 99 ? entry.relativePath.slice(0, 99) : entry.relativePath,
      data.length,
      entry.isDirectory ? '5' : '0',
      entry.isDirectory ? '5' : '0'
    );
    blocks.push(header);
    if (data.length > 0) {
      blocks.push(padToBlock(data));
    }
  }
  // End-of-archive marker: two 512-byte zero blocks
  blocks.push(new Uint8Array(1024));
  const tarData = concatenateBuffers(blocks);
  // Gzip using CompressionStream (available in modern Chromium/Electron)
  const cs = new CompressionStream('gzip');
  const writer = cs.writable.getWriter();
  writer.write(tarData);
  writer.close();
  const reader = cs.readable.getReader();
  const chunks = [];
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
  }
  return concatenateBuffers(chunks).buffer;
}

function createTarHeader(name, size, fileType, typeFlag) {
  const header = new Uint8Array(512);
  const enc = new TextEncoder();
  // name (0-99)
  const nameBytes = enc.encode(name.slice(0, 99));
  header.set(nameBytes, 0);
  // mode (100-107)
  header.set(enc.encode('0000755\0'), 100);
  // uid (108-115)
  header.set(enc.encode('0001000\0'), 108);
  // gid (116-123)
  header.set(enc.encode('0001000\0'), 116);
  // size (124-135)
  header.set(enc.encode(size.toString(8).padStart(11, '0') + '\0'), 124);
  // mtime (136-147)
  const mtime = Math.floor(Date.now() / 1000);
  header.set(enc.encode(mtime.toString(8).padStart(11, '0') + '\0'), 136);
  // typeflag (156)
  header[156] = enc.encode(typeFlag || '0')[0];
  // magic (257-262)
  header.set(enc.encode('ustar\0'), 257);
  // version (263-264)
  header.set(enc.encode('00'), 263);
  // checksum (148-155) - compute
  header.set(enc.encode('        '), 148); // 8 spaces placeholder
  let checksum = 0;
  for (let i = 0; i < 512; i++) {
    checksum += header[i];
  }
  header.set(enc.encode(checksum.toString(8).padStart(6, '0') + '\0 '), 148);
  return header;
}

function padToBlock(data) {
  const remainder = data.length % 512;
  if (remainder === 0) return data;
  const padded = new Uint8Array(data.length + (512 - remainder));
  padded.set(data);
  return padded;
}

function concatenateBuffers(arrays) {
  let total = 0;
  for (const arr of arrays) total += arr.length;
  const result = new Uint8Array(total);
  let offset = 0;
  for (const arr of arrays) {
    result.set(arr, offset);
    offset += arr.length;
  }
  return result;
}

async function downloadWorkspaceEntry(path) {
  if (!state.selected) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  setSessionActivity('正在下载...');
  try {
    const basename = normalizeWorkspacePath(path).split('/').pop() || 'workspace';
    const result = await window.stellacode.downloadWorkspace({
      serverId,
      conversationId,
      path: normalizeWorkspacePath(path),
      suggestedName: `${basename}.tar.gz`
    });
    if (result.saved) {
      setSessionActivity(`已保存到 ${result.filePath}`, { clearAfterMs: 3000 });
    } else {
      setSessionActivity('下载已取消', { clearAfterMs: 1500 });
    }
  } catch (error) {
    showToast(`下载失败: ${error.message}`);
    setSessionActivity('下载失败', { clearAfterMs: 2000 });
  }
}

function workspaceListing(key, path) {
  return state.workspaceListings.get(workspaceCacheKey(key, path));
}

function workspaceError(key, path) {
  return state.workspaceErrors.get(workspaceCacheKey(key, path));
}

function workspaceIsLoading(key, path) {
  return state.workspaceLoading.has(workspaceCacheKey(key, path));
}

function workspaceFilePreview(key, path) {
  return state.workspaceFilePreviews.get(workspaceFileCacheKey(key, path));
}

function workspaceFileError(key, path) {
  return state.workspaceFileErrors.get(workspaceFileCacheKey(key, path));
}

function workspaceFileIsLoading(key, path) {
  return state.workspaceFileLoading.has(workspaceFileCacheKey(key, path));
}

function fileTabKey(path) {
  return `${selectedKey()}:${normalizeWorkspacePath(path)}`;
}

function fileNameFromPath(path) {
  return normalizeWorkspacePath(path).split('/').filter(Boolean).at(-1) || normalizeWorkspacePath(path) || 'file';
}

function fileExtension(path) {
  const name = fileNameFromPath(path).toLowerCase();
  const index = name.lastIndexOf('.');
  return index >= 0 ? name.slice(index + 1) : '';
}

function isMarkdownFile(path) {
  return ['md', 'markdown', 'mdown'].includes(fileExtension(path));
}

function isImageFile(path) {
  return ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg', 'avif'].includes(fileExtension(path));
}

function isImageAttachment(attachment) {
  const mediaType = safeText(attachment?.media_type).toLowerCase();
  return attachment?.kind === 'image' || mediaType.startsWith('image/') || isImageFile(attachment?.path || attachment?.name || '');
}

function imageMimeType(path) {
  const ext = fileExtension(path);
  if (ext === 'jpg') return 'image/jpeg';
  if (ext === 'svg') return 'image/svg+xml';
  return ext ? `image/${ext}` : 'application/octet-stream';
}

function attachmentMimeType(attachment) {
  return safeText(attachment?.media_type) || imageMimeType(attachment?.name || attachment?.path || '');
}

function attachmentThumbnailUrl(attachment) {
  return safeText(attachment?.thumbnail?.data_url);
}

function attachmentMarkerIndexes(value) {
  const indexes = new Set();
  const text = safeText(value);
  const pattern = /\[\[attachment:(\d+)]]/g;
  let match;
  while ((match = pattern.exec(text)) !== null) {
    indexes.add(Number(match[1]));
  }
  return indexes;
}

function messageAttachmentMarkerText(message, detail) {
  const parts = [];
  const explicit = detail?.text_with_attachment_markers || message?.text_with_attachment_markers;
  if (explicit) {
    parts.push(explicit);
  }
  const items = Array.isArray(detail?.items) ? detail.items : Array.isArray(message?.items) ? message.items : [];
  items.forEach((item) => {
    if (item?.text_with_attachment_markers) {
      parts.push(item.text_with_attachment_markers);
    }
  });
  return parts.join('\n');
}

function messageInlineAttachmentIndexes(message, detail) {
  return attachmentMarkerIndexes(messageAttachmentMarkerText(message, detail));
}

function messageNeedsFullDetail(message) {
  return (
    (Number(message?.attachment_count || 0) > 0 && (!Array.isArray(message?.attachments) || message.attachments.length === 0)) ||
    Boolean(message?.has_attachment_errors)
  );
}

function preferredFileViewMode(path) {
  if (isMarkdownFile(path)) {
    return 'preview';
  }
  return 'source';
}

function currentFileViewMode(path) {
  const key = fileTabKey(path);
  return state.fileViewModes.get(key) || preferredFileViewMode(path);
}

function setFileViewMode(path, mode) {
  state.fileViewModes.set(fileTabKey(path), mode);
}

function ensureFileTab(path) {
  const normalized = normalizeWorkspacePath(path);
  if (!normalized) {
    return;
  }
  state.fileTabs = state.fileTabs.filter((tab) => tab.key.startsWith(`${selectedKey()}:`));
  const key = fileTabKey(normalized);
  if (!state.fileTabs.some((tab) => tab.key === key)) {
    state.fileTabs.push({ key, path: normalized });
  }
}

function selectedFileTabs() {
  const prefix = `${selectedKey()}:`;
  return state.fileTabs.filter((tab) => tab.key.startsWith(prefix));
}

function shouldHideFileBarForPreview() {
  if (!state.fileBarOpen) {
    return false;
  }
  const workbenchWidth = document.querySelector('.workbench')?.getBoundingClientRect().width || 0;
  const minimumChatWidth = 560;
  return workbenchWidth > 0 && workbenchWidth < state.layout.context + state.layout.file + minimumChatWidth;
}

function sidebarResponsiveWidth() {
  if (state.settings?.sidebarCollapsed) {
    return 54;
  }
  const width = window.innerWidth || 0;
  const stored = state.layout.sidebar;
  if (width && state.fileBarOpen && width < 1800) {
    return clampNumber(stored, 180, 220);
  }
  if (width && width < 1500) {
    return clampNumber(stored, 180, 240);
  }
  return clampNumber(stored, 220, 420);
}

function panelResponsiveWidth(value, min, max) {
  const width = window.innerWidth || 0;
  const panelMax = width && width < 1500 ? Math.min(max, 280) : max;
  return clampNumber(value, min, panelMax);
}

function closeFileTab(path) {
  const normalized = normalizeWorkspacePath(path);
  const key = fileTabKey(normalized);
  const index = state.fileTabs.findIndex((tab) => tab.key === key);
  state.fileTabs = state.fileTabs.filter((tab) => tab.key !== key);
  if (state.activePreviewFilePath === normalized) {
    const tabs = selectedFileTabs();
    const next = tabs[Math.max(0, index - 1)] || tabs[0] || null;
    state.activePreviewFilePath = next?.path || null;
  }
  if (selectedFileTabs().length === 0 && state.activeContextTab === 'file') {
    state.activePreviewFilePath = null;
    state.activeContextTab = 'overview';
    state.contextCollapsed = true;
  }
  renderFilesContext();
  renderContext();
}

function workspaceExpandedSet(key) {
  let expanded = state.workspaceExpandedPaths.get(key);
  if (!expanded) {
    expanded = new Set(['']);
    state.workspaceExpandedPaths.set(key, expanded);
  }
  return expanded;
}

function normalizeWorkspacePath(value) {
  return safeText(value)
    .replaceAll('\\', '/')
    .split('/')
    .filter((part) => part && part !== '.')
    .join('/');
}

function displayConversationName(serverId, conversation) {
  const key = conversationKey(serverId, conversation.conversation_id);
  return (
    safeText(conversation.nickname).trim() ||
    state.settings.conversationNames[key] ||
    conversation.platform_chat_id ||
    conversation.conversation_id
  );
}

function displayConversationModel(conversation, status) {
  const model = status?.model || conversation?.model || '';
  if (status?.model_selection_pending || conversation?.model_selection_pending) {
    return 'pending';
  }
  return model;
}

function ensureLocalSettings() {
  state.settings.conversationNames ||= {};
  state.settings.hiddenConversations ||= {};
  state.settings.invalidModelAliases ||= {};
  state.settings.sidebarCollapsed = Boolean(state.settings.sidebarCollapsed);
}

function isConversationHidden(serverId, conversationId) {
  return Boolean(state.settings?.hiddenConversations?.[conversationKey(serverId, conversationId)]);
}

function isInvalidModelAlias(model) {
  return Boolean(model && state.settings?.invalidModelAliases?.[model]);
}

function rememberInvalidModelAlias(model) {
  if (!model) {
    return;
  }
  ensureLocalSettings();
  state.settings.invalidModelAliases[model] = true;
  saveSettingsSoon();
}

function invalidModelFromError(error) {
  return safeText(error?.message).match(/unknown model alias\s+([^\s]+)/i)?.[1] || '';
}

function addModelCandidate(candidates, seen, model) {
  if (!model || seen.has(model) || isInvalidModelAlias(model)) {
    return;
  }
  seen.add(model);
  candidates.push(model);
}

function modelCandidatesFor(serverId) {
  const candidates = [];
  const seen = new Set();
  if (state.selected?.serverId === serverId) {
    const selectedStatus = state.statuses.get(selectedKey());
    const selectedConversation = currentConversation();
    if (!selectedStatus?.model_selection_pending) {
      addModelCandidate(candidates, seen, selectedStatus?.model);
    }
    if (!selectedConversation?.model_selection_pending) {
      addModelCandidate(candidates, seen, selectedConversation?.model);
    }
  }
  for (const [key, status] of state.statuses) {
    if (key.startsWith(`${serverId}:`) && !status?.model_selection_pending) {
      addModelCandidate(candidates, seen, status?.model);
    }
  }
  for (const conversation of visibleConversations(serverId)) {
    if (!conversation.model_selection_pending) {
      addModelCandidate(candidates, seen, conversation.model);
    }
  }
  return candidates;
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

function emptyTokenUsage() {
  return {
    cacheRead: 0,
    cacheWrite: 0,
    input: 0,
    output: 0,
    totalTokens: 0,
    cost: 0
  };
}

function normalizeTokenUsage(source) {
  const usage =
    source?.token_usage ||
    source?.message?.token_usage ||
    source?.usage ||
    source?.tokens ||
    source?.response_usage ||
    null;
  if (!usage || typeof usage !== 'object' || Array.isArray(usage)) {
    return emptyTokenUsage();
  }
  const cacheRead = Number(usage.cache_read ?? usage.cacheRead ?? usage.cached_input_tokens ?? usage.cache_read_tokens ?? 0);
  const cacheWrite = Number(usage.cache_write ?? usage.cacheWrite ?? usage.cache_write_tokens ?? 0);
  const input = Number(usage.input ?? usage.input_tokens ?? usage.prompt_tokens ?? usage.uncache_input ?? 0);
  const output = Number(usage.output ?? usage.output_tokens ?? usage.completion_tokens ?? 0);
  const totalTokens = Number(usage.total ?? usage.total_tokens ?? 0) || cacheRead + cacheWrite + input + output;
  const costValue = usage.cost;
  const cost =
    typeof costValue === 'object' && costValue
      ? Object.values(costValue).reduce((sum, value) => sum + Number(value || 0), 0)
      : Number(costValue || usage.cost_usd || 0);
  return {
    cacheRead,
    cacheWrite,
    input,
    output,
    totalTokens,
    cost
  };
}

function messageTokenUsage(message, detail = state.messageDetails.get(message?.id)) {
  const detailUsage = normalizeTokenUsage(detail);
  if (detailUsage.totalTokens > 0 || detailUsage.cost > 0) {
    return detailUsage;
  }
  return normalizeTokenUsage(message);
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
  rendered = rendered.replace(/!\[([^\]\n]*)\]\((https?:\/\/[^\s)]+)\)/gi, (_match, label, href) => {
    const safeHref = safeLinkHref(href);
    if (!safeHref) {
      return '';
    }
    return `<img class="message-inline-image" src="${escapeHtml(safeHref)}" alt="${escapeHtml(label)}" loading="lazy" />`;
  });
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

function toolSummary(kind, name, payload) {
  const text = safeText(payload).trim();
  if (!text) {
    return kind === 'call' ? '准备调用工具' : '工具已返回';
  }
  try {
    const parsed = JSON.parse(text);
    if (parsed.command) {
      return parsed.command;
    }
    if (parsed.stdout) {
      return String(parsed.stdout).trim().split('\n')[0] || '输出为空';
    }
    if (parsed.success !== undefined) {
      return parsed.success ? '执行成功' : '执行失败';
    }
  } catch {
    // Non-JSON tool text is still useful as the summary.
  }
  return text.split('\n').find(Boolean)?.slice(0, 120) || name;
}

function formatToolPayload(payload) {
  if (payload && typeof payload === 'object') {
    return JSON.stringify(payload, null, 2);
  }
  const text = safeText(payload).trim();
  if (!text) {
    return '';
  }
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

function parseToolPayload(payload) {
  const text = safeText(payload).trim();
  if (!text) {
    return { text: '', parsed: null };
  }
  try {
    return { text, parsed: JSON.parse(text) };
  } catch {
    return { text, parsed: null };
  }
}

function toolValue(parsed, keys) {
  if (!parsed || typeof parsed !== 'object') {
    return '';
  }
  for (const key of keys) {
    if (parsed[key] !== undefined && parsed[key] !== null && parsed[key] !== '') {
      return parsed[key];
    }
  }
  return '';
}

function renderToolField(label, value, options = {}) {
  const text = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
  if (!safeText(text).trim()) {
    return '';
  }
  const tag = options.code ? 'pre' : 'div';
  const className = options.error ? 'tool-detail-value error' : 'tool-detail-value';
  return `
    <section class="tool-detail-section">
      <div class="tool-detail-label">${escapeHtml(label)}</div>
      <${tag} class="${className}"><code>${escapeHtml(text)}</code></${tag}>
    </section>
  `;
}

function renderToolDetails(kind, name, payload) {
  const { text, parsed } = parseToolPayload(payload);
  if (!text) {
    return '';
  }
  const command = toolValue(parsed, ['command', 'cmd', 'input', 'script']);
  const cwd = toolValue(parsed, ['cwd', 'workdir', 'working_dir']);
  const stdout = toolValue(parsed, ['stdout', 'output']);
  const stderr = toolValue(parsed, ['stderr', 'error']);
  const exitCode = toolValue(parsed, ['exit_code', 'exitCode', 'code', 'status']);
  const success = toolValue(parsed, ['success']);

  if (kind === 'call') {
    const fields = [
      renderToolField('命令', command || text, { code: true }),
      renderToolField('目录', cwd),
      parsed && !command ? renderToolField('参数', parsed, { code: true }) : ''
    ].join('');
    return `<div class="tool-detail">${fields}</div>`;
  }

  const fields = [
    stdout ? renderToolField('输出', stdout, { code: true }) : '',
    stderr ? renderToolField('错误', stderr, { code: true, error: true }) : '',
    exitCode !== '' ? renderToolField('退出码', String(exitCode)) : '',
    success !== '' ? renderToolField('状态', success ? '成功' : '失败') : ''
  ].join('');
  if (fields.trim()) {
    return `<div class="tool-detail">${fields}</div>`;
  }
  return `<div class="tool-detail">${renderToolField(kind === 'result' ? '结果' : name, parsed || text, { code: true })}</div>`;
}

function renderToolCard(kind, name, payload) {
  const label = kind === 'call' ? '调用工具' : '工具结果';
  const details = renderToolDetails(kind, name, payload);
  const icon = kind === 'call' ? icons.terminal : icons.check;
  return `
    <details class="tool-card ${kind === 'result' ? 'result' : 'call'}"${details ? '' : ' open'}>
      <summary class="tool-card-head">
        <span class="tool-card-icon">${icon}</span>
        <span class="tool-card-label">${label}</span>
        <code>${escapeHtml(name)}</code>
        <span class="tool-card-summary">${escapeHtml(toolSummary(kind, name, payload))}</span>
        <span class="tool-card-chevron">${icons.chevronRight}</span>
      </summary>
      ${details || ''}
    </details>
  `;
}

function renderAttachmentMarker(index, attachments) {
  const attachment = attachments?.[index];
  if (!attachment) {
    return '';
  }
  return renderMessageAttachmentItem(attachment);
}

function renderMarkdownWithAttachmentMarkers(value, attachments = []) {
  const text = safeText(value);
  if (!text.includes('[[attachment:')) {
    return renderMarkdownWithToolBlocks(text);
  }
  const parts = [];
  const pattern = /\[\[attachment:(\d+)]]/g;
  let cursor = 0;
  let match;
  while ((match = pattern.exec(text)) !== null) {
    const before = text.slice(cursor, match.index);
    if (before.trim()) {
      parts.push(renderMarkdownWithToolBlocks(before));
    }
    const attachmentHtml = renderAttachmentMarker(Number(match[1]), attachments);
    if (attachmentHtml) {
      parts.push(`<div class="message-inline-attachment">${attachmentHtml}</div>`);
    }
    cursor = match.index + match[0].length;
  }
  const rest = text.slice(cursor);
  if (rest.trim()) {
    parts.push(renderMarkdownWithToolBlocks(rest));
  }
  return parts.join('');
}

function renderMarkdownWithToolBlocks(value) {
  const text = safeText(value);
  const pattern = /\[tool_(call|result)\s+([^\]\n]+)\]\s*([\s\S]*?)(?=\n{2,}\S|\n\[tool_(?:call|result)\s+|$)/g;
  const parts = [];
  let cursor = 0;
  let match;
  while ((match = pattern.exec(text)) !== null) {
    const before = text.slice(cursor, match.index);
    if (before.trim()) {
      parts.push(markdownToHtml(before));
    }
    parts.push(renderToolCard(match[1], match[2], match[3]));
    cursor = match.index + match[0].length;
  }
  const rest = text.slice(cursor);
  if (rest.trim()) {
    parts.push(markdownToHtml(rest));
  }
  return parts.join('');
}

function renderMarkdownMessage(value, attachments = []) {
  const text = safeText(value).trim();
  if (!text) {
    return '<span class="message-empty">空消息</span>';
  }
  const blocks = text.split(/(```[\s\S]*?```)/g);
  return blocks
    .map((block) => {
      if (block.startsWith('```') && block.endsWith('```')) {
        return markdownToHtml(block);
      }
      return renderMarkdownWithAttachmentMarkers(block, attachments);
    })
    .join('');
}

function renderStructuredMessageContent(message, detail, options = {}) {
  const items = Array.isArray(detail?.items) ? detail.items : Array.isArray(message?.items) ? message.items : [];
  const attachments = detail?.attachments || message?.attachments || [];
  if (items.length === 0) {
    return renderMarkdownMessage(
      detail?.text_with_attachment_markers ||
        message?.text_with_attachment_markers ||
        detail?.rendered_text ||
        message?.text ||
        message?.preview ||
        '',
      attachments
    );
  }
  const key = selectedKey();
  const parts = items
    .map((item) => {
      if (item.type === 'text') {
        if (options.plainSyntheticSummary) {
          return `<span class="synthetic-message-summary">${escapeHtml(item.text || '')}</span>`;
        }
        return renderMarkdownMessage(item.text_with_attachment_markers || item.text || '', attachments);
      }
      if (item.type === 'file') {
        if (options.hideFiles) {
          return '';
        }
        return renderMessageAttachmentItem(attachments[item.attachment_index], key);
      }
      if (item.type === 'tool_call') {
        return renderToolCard('call', item.tool_name || 'tool', formatToolPayload(item.arguments));
      }
      if (item.type === 'tool_result') {
        const payload =
          item.context_with_attachment_markers ||
          item.context ||
          (item.file_attachment_index !== undefined ? JSON.stringify({ file_attachment_index: item.file_attachment_index }) : '');
        return renderToolCard('result', item.tool_name || 'tool', payload);
      }
      return '';
    })
    .filter(Boolean);
  if (parts.length === 0) {
    return renderMarkdownMessage(
      detail?.text_with_attachment_markers ||
        message?.text_with_attachment_markers ||
        detail?.rendered_text ||
        message?.text ||
        message?.preview ||
        '',
      attachments
    );
  }
  return parts.join('');
}

function conversationHeaderSubtitle(server, conversation, status) {
  const model = status?.model || conversation?.model || 'model pending';
  if (isRemoteStatus(status)) {
    return `${status.remote} · ${model}`;
  }
  return `${server?.name || state.selected.serverId} · ${model}`;
}

function refreshConversationStatusLater(serverId, conversationId, refreshEpoch) {
  api(serverId, `/api/conversations/${conversationId}/status`)
    .then((status) => {
      if (refreshEpoch !== state.refreshEpoch || state.selected?.conversationId !== conversationId) {
        return;
      }
      const key = conversationKey(serverId, conversationId);
      const previous = state.statuses.get(key);
      const workspaceChanged =
        previous &&
        (safeText(previous.remote) !== safeText(status.data.remote) ||
          safeText(previous.workspace) !== safeText(status.data.workspace));
      state.statuses.set(key, status.data);
      if (workspaceChanged && selectedConnectionMatches(serverId, conversationId)) {
        invalidateWorkspaceCache(key);
        refreshWorkspace().catch(() => {});
      }
      renderSidebar();
      renderHeader();
      renderContext();
    })
    .catch(() => {
      if (refreshEpoch === state.refreshEpoch && state.selected?.conversationId === conversationId) {
        state.statuses.delete(conversationKey(serverId, conversationId));
        renderSidebar();
        renderHeader();
      }
    });
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

function clampNumber(value, min, max) {
  return Math.min(max, Math.max(min, Number(value) || min));
}

function loadLayoutSettings() {
  try {
    const saved = JSON.parse(localStorage.getItem(layoutStorageKey) || '{}');
    state.layout = {
      sidebar: clampNumber(saved.sidebar ?? state.layout.sidebar, 220, 420),
      context: clampNumber(saved.context ?? state.layout.context, 260, 560),
      file: clampNumber(saved.file ?? state.layout.file, 280, 620),
      terminal: clampNumber(saved.terminal ?? state.layout.terminal, 180, 520)
    };
  } catch {
    localStorage.removeItem(layoutStorageKey);
  }
}

function saveLayoutSettings() {
  localStorage.setItem(layoutStorageKey, JSON.stringify(state.layout));
}

function applyLayoutSettings() {
  const collapsed = Boolean(state.settings?.sidebarCollapsed);
  document.body.classList.toggle('sidebar-collapsed', collapsed);
  if (elements.toggleSidebarButton) {
    elements.toggleSidebarButton.title = collapsed ? '显示 Conversation 列表' : '隐藏 Conversation 列表';
    elements.toggleSidebarButton.setAttribute('aria-label', elements.toggleSidebarButton.title);
    elements.toggleSidebarButton.setAttribute('aria-pressed', collapsed ? 'true' : 'false');
  }
  document.querySelector('.app-shell')?.style.setProperty('--sidebar-width', `${sidebarResponsiveWidth()}px`);
  const workbench = document.querySelector('.workbench');
  workbench?.style.setProperty('--inspector-size', `${panelResponsiveWidth(state.layout.context, 240, 620)}px`);
  workbench?.style.setProperty('--file-size', `${panelResponsiveWidth(state.layout.file, 260, 680)}px`);
  workbench?.style.setProperty('--terminal-size', `${state.layout.terminal}px`);
  fitActiveXterm();
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
    await mapLimit(
      conversations.filter((conversation) => !isConversationHidden(serverId, conversation.conversation_id)).slice(0, 60),
      4,
      async (conversation) => {
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
  clearActivePoll();
  clearWebsocketSubscription();
  clearTerminalPoll();
  disposeXtermSessions();
  state.refreshEpoch += 1;
  state.selected = { serverId, conversationId };
  state.activeServerId = serverId;
  state.settings.activeServerId = serverId;
  const key = selectedKey();
  if (!state.workspacePaths.has(key)) {
    state.workspacePaths.set(key, '');
  }
  workspaceExpandedSet(key);
  state.messages = [];
  state.messagePageStart = 0;
  state.messagePageTotal = 0;
  state.loadingOlderMessages = false;
  state.optimisticMessages = state.optimisticMessages.filter(
    (message) => message.conversationKey !== conversationKey(serverId, conversationId)
  );
  state.messagesSignature = '';
  state.messageDetails.clear();
  state.messageDetailsLoading.clear();
  state.expandedMessages.clear();
  state.expandedExecutionGroups.clear();
  state.channelEvents = state.channelEvents.filter((event) => event.conversationKey === conversationKey(serverId, conversationId));
  state.sessionProgress = null;
  state.activePreviewMessageId = null;
  state.activePreviewFilePath = null;
  state.activeTerminalId = null;
  setSessionActivity('');
  saveSettingsSoon();
  renderSidebar();
  renderHeader();
  renderMessages();
  renderContext();
  await refreshConversation();
  connectConversationWebsocket(serverId, conversationId);
  await refreshWorkspace();
}

async function refreshConversation() {
  if (!state.selected) {
    renderHeader();
    renderMessages();
    return;
  }
  const refreshEpoch = state.refreshEpoch;
  const { serverId, conversationId } = state.selected;
  let shouldRenderMessages = false;
  setRefreshing(true);
  try {
    let messages = state.messages.length > 0
      ? await fetchConversationMessages(serverId, conversationId, { incremental: true, limit: MESSAGE_PAGE_LIMIT })
      : await fetchInitialConversationMessages(serverId, conversationId);
    if (refreshEpoch !== state.refreshEpoch || state.selected?.conversationId !== conversationId) {
      setRefreshing(false);
      return;
    }
    let fetchedMessages = messages.data?.messages || [];
    const lastKnownId = lastMessageId();
    const expectedOffset = lastKnownId ? Number(lastKnownId) + 1 : 0;
    if (lastKnownId && Number(messages.data?.offset || 0) !== expectedOffset) {
      messages = await fetchInitialConversationMessages(serverId, conversationId);
      if (refreshEpoch !== state.refreshEpoch || state.selected?.conversationId !== conversationId) {
        setRefreshing(false);
        return;
      }
      fetchedMessages = messages.data?.messages || [];
      state.messages = fetchedMessages;
      state.messagePageStart = Number(messages.data?.offset || 0);
      state.messagePageTotal = Number(messages.data?.total || fetchedMessages.length);
    } else if (fetchedMessages.length > 0) {
      const wasEmpty = state.messages.length === 0;
      state.messages = [...state.messages, ...fetchedMessages];
      state.messagePageStart = wasEmpty
        ? Number(messages.data?.offset || 0)
        : Math.min(state.messagePageStart || Number(messages.data?.offset || 0), firstMessageIndex(state.messages));
      state.messagePageTotal = Number(messages.data?.total || state.messages.length);
    } else if (state.messages.length === 0) {
      state.messagePageStart = Number(messages.data?.offset || 0);
      state.messagePageTotal = Number(messages.data?.total || 0);
    }
    const nextSignature = messageListSignature(state.messages);
    const optimisticBefore = state.optimisticMessages.length;
    state.optimisticMessages = state.optimisticMessages.filter((message) => {
      if (message.conversationKey !== conversationKey(serverId, conversationId)) {
        return true;
      }
      return !serverHasOptimisticMessage(fetchedMessages, message);
    });
    if (nextSignature !== state.messagesSignature) {
      state.messagesSignature = nextSignature;
      shouldRenderMessages = true;
    } else if (state.optimisticMessages.length !== optimisticBefore) {
      shouldRenderMessages = true;
    }
    state.lastRefreshAt = new Date().toISOString();
    refreshConversationStatusLater(serverId, conversationId, refreshEpoch);
  } catch (error) {
    if (refreshEpoch !== state.refreshEpoch) {
      setRefreshing(false);
      return;
    }
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
    renderMessages({ stickToBottom: true });
  }
  renderContext();
  if (refreshEpoch === state.refreshEpoch) {
    setRefreshing(false);
  }
}

async function loadOlderMessages() {
  if (!state.selected || state.loadingOlderMessages || !messagePageHasOlder()) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  const key = selectedKey();
  const oldScrollHeight = elements.messageList.scrollHeight;
  const oldScrollTop = elements.messageList.scrollTop;
  const currentStart = firstMessageIndex();
  const offset = Math.max(0, currentStart - MESSAGE_PAGE_LIMIT);
  const limit = currentStart - offset;
  state.loadingOlderMessages = true;
  try {
    const response = await fetchConversationMessages(serverId, conversationId, {
      incremental: false,
      offset,
      limit
    });
    if (selectedKey() !== key) {
      return;
    }
    const olderMessages = response.data?.messages || [];
    if (olderMessages.length > 0) {
      const existingIds = new Set(state.messages.map((message) => message.id));
      state.messages = [
        ...olderMessages.filter((message) => !existingIds.has(message.id)),
        ...state.messages
      ];
      state.messagePageStart = Number(response.data?.offset || offset);
      state.messagePageTotal = Number(response.data?.total || state.messagePageTotal || state.messages.length);
      state.messagesSignature = messageListSignature(state.messages);
      renderMessages({ preserveScroll: true });
      elements.messageList.scrollTop = oldScrollTop + (elements.messageList.scrollHeight - oldScrollHeight);
    }
  } catch (error) {
    showToast(`加载历史消息失败：${error.message}`);
  } finally {
    if (selectedKey() === key) {
      state.loadingOlderMessages = false;
    }
  }
}

async function refreshWorkspace(path = currentWorkspacePath(), options = {}) {
  if (!state.selected) {
    return;
  }
  const { expand = false, setActive = true } = options;
  const { serverId, conversationId } = state.selected;
  const key = selectedKey();
  const nextPath = normalizeWorkspacePath(path);
  const cacheKey = workspaceCacheKey(key, nextPath);
  if (setActive) {
    state.workspacePaths.set(key, nextPath);
  }
  if (expand) {
    workspaceExpandedSet(key).add(nextPath);
  }
  state.workspaceLoading.add(cacheKey);
  state.workspaceErrors.delete(cacheKey);
  if (!state.workspaceListings.has(cacheKey) && (state.activeContextTab === 'overview' || state.fileBarOpen)) {
    renderContext();
  }
  try {
    const response = await api(
      serverId,
      `/api/conversations/${conversationId}/workspace?path=${encodeURIComponent(nextPath)}&limit=300`
    );
    const listingPath = normalizeWorkspacePath(response.data?.path || nextPath);
    state.workspaceListings.set(cacheKey, response.data);
    state.workspaceListings.set(workspaceCacheKey(key, listingPath), response.data);
    if (setActive) {
      state.workspacePaths.set(key, listingPath);
    }
    if (expand) {
      workspaceExpandedSet(key).add(listingPath);
    }
  } catch (error) {
    state.workspaceErrors.set(cacheKey, error.message);
  } finally {
    state.workspaceLoading.delete(cacheKey);
    if (selectedKey() === key && (state.activeContextTab === 'overview' || state.fileBarOpen)) {
      renderContext();
    }
  }
}

async function fetchMessageDetail(messageId) {
  if (!state.selected || state.messageDetails.has(messageId) || state.messageDetailsLoading.has(messageId)) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  state.messageDetailsLoading.add(messageId);
  if (state.activePreviewMessageId === messageId) {
    renderContext();
  }
  try {
    const response = await api(serverId, `/api/conversations/${conversationId}/messages/${messageId}`);
    state.messageDetails.set(messageId, response.data);
  } finally {
    state.messageDetailsLoading.delete(messageId);
  }
}

async function fetchWorkspaceFile(path, options = {}) {
  if (!state.selected) {
    return;
  }
  const { renderContextOnChange = true, renderMessagesOnChange = false, limitBytes = 65536, apiPath = '' } = options;
  const { serverId, conversationId } = state.selected;
  const key = selectedKey();
  const normalized = normalizeWorkspacePath(path);
  const cacheKey = workspaceFileCacheKey(key, normalized);
  if (state.workspaceFilePreviews.has(cacheKey) || state.workspaceFileLoading.has(cacheKey)) {
    return;
  }
  state.workspaceFileLoading.add(cacheKey);
  state.workspaceFileErrors.delete(cacheKey);
  if (renderContextOnChange) {
    renderContext();
  }
  try {
    const requestPath =
      apiPath ||
      `/api/conversations/${conversationId}/workspace/file?path=${encodeURIComponent(normalized)}&offset=0&limit_bytes=${encodeURIComponent(limitBytes)}`;
    const response = await api(serverId, appendFileLimitQuery(requestPath, limitBytes));
    if (selectedKey() !== key) {
      return;
    }
    state.workspaceFilePreviews.set(cacheKey, response.data);
  } catch (error) {
    state.workspaceFileErrors.set(cacheKey, error.message);
  } finally {
    state.workspaceFileLoading.delete(cacheKey);
    if (selectedKey() === key) {
      if (renderContextOnChange) {
        renderContext();
      }
      if (renderMessagesOnChange) {
        renderMessages();
      }
    }
  }
}

function appendFileLimitQuery(path, limitBytes) {
  if (!path || !path.includes('/workspace/file')) {
    return path;
  }
  const joiner = path.includes('?') ? '&' : '?';
  const withOffset = /[?&]offset=/.test(path) ? path : `${path}${joiner}offset=0`;
  const limitJoiner = withOffset.includes('?') ? '&' : '?';
  return /[?&]limit_bytes=/.test(withOffset)
    ? withOffset
    : `${withOffset}${limitJoiner}limit_bytes=${encodeURIComponent(limitBytes)}`;
}

async function toggleMessage(messageId) {
  const message = state.messages.find((item) => item.id === messageId);
  const isSynthetic = isSyntheticMediaContextMessage(message);
  if (!isSynthetic) {
    return;
  }
  if (isSynthetic && state.expandedMessages.has(messageId)) {
    state.expandedMessages.delete(messageId);
  } else if (isSynthetic) {
    state.expandedMessages.add(messageId);
  }
  updateMessageArticle(messageId);
}

async function selectWorkspaceFile(path) {
  const normalized = normalizeWorkspacePath(path);
  state.activePreviewFilePath = normalized;
  state.activePreviewMessageId = null;
  ensureFileTab(normalized);
  state.activeContextTab = 'file';
  state.fileBarOpen = true;
  state.contextCollapsed = true;
  renderContext();
  await fetchWorkspaceFile(normalized);
}

async function createConversation(serverId, localName) {
  const platform_chat_id = createId('stellacode');
  const nickname = localName.trim();
  const response = await api(serverId, '/api/conversations', {
    method: 'POST',
    body: {
      platform_chat_id,
      ...(nickname ? { nickname } : {})
    }
  });
  const conversationId = response.data.conversation_id;
  closeModal();
  await refreshServer(serverId);
  await selectConversation(serverId, conversationId);
}

async function postConversationMessage(serverId, conversationId, text) {
  return api(serverId, `/api/conversations/${conversationId}/messages`, {
    method: 'POST',
    body: {
      user_name: 'Stellacode',
      text
    }
  });
}

async function recoverModelAndSend(serverId, conversationId, originalText, originalError) {
  let models = [];
  try {
    models = (await fetchModels(serverId)).map(modelAlias).filter(Boolean);
  } catch {
    models = modelCandidatesFor(serverId);
  }
  for (const model of models) {
    try {
      await postConversationMessage(serverId, conversationId, `/model ${model}`);
      await postConversationMessage(serverId, conversationId, originalText);
      return;
    } catch (error) {
      const rejectedModel = invalidModelFromError(error);
      if (!rejectedModel) {
        throw error;
      }
      rememberInvalidModelAlias(rejectedModel);
    }
  }
  throw originalError;
}

async function openModelPicker() {
  if (!state.selected) {
    showToast('先选择一个 Conversation');
    return;
  }
  const { serverId, conversationId } = state.selected;
  let models = [];
  try {
    models = await fetchModels(serverId);
  } catch (error) {
    showToast(`需要后端提供 GET /api/models: ${error.message}`);
    return;
  }
  const rows = models
    .map(
      (model) => {
        const alias = modelAlias(model);
        return `
        <button class="choice-row" type="button" data-model-select="${escapeHtml(alias)}">
          <span>
            <strong>${escapeHtml(alias)}</strong>
            <small>${escapeHtml(model.model_name || '')}</small>
          </span>
        </button>
      `;
      }
    )
    .join('');
  openModal(`
    <div class="modal-card small">
      <div class="modal-head">
        <h2>选择模型</h2>
        <button class="icon-button" type="button" data-close-modal>×</button>
      </div>
      <div class="choice-list">${rows || '<div class="empty-state compact">后端没有可选模型</div>'}</div>
    </div>
  `);
  elements.modalLayer.querySelectorAll('[data-model-select]').forEach((button) => {
    button.addEventListener('click', async () => {
      const model = button.dataset.modelSelect;
      closeModal();
      try {
        await postConversationMessage(serverId, conversationId, `/model ${model}`);
        await refreshConversation();
        pollActiveConversation();
      } catch (error) {
        showToast(error.message);
      }
    });
  });
}

async function sendMessage() {
  if (!state.selected) {
    return;
  }
  const text = elements.composerInput.value.trim();
  if (!text) {
    return;
  }
  if (text === '/model') {
    await openModelPicker();
    return;
  }
  elements.composerInput.value = '';
  autosizeComposer();
  elements.sendButton.disabled = true;
  const { serverId, conversationId } = state.selected;
  const localKey = conversationKey(serverId, conversationId);
  const status = state.statuses.get(localKey);
  const conversation = currentConversation();
  const isControlCommand = text.startsWith('/');
  const needsModel =
    (status?.model_selection_pending || conversation?.model_selection_pending) && !text.startsWith('/model');
  const optimistic = {
    id: createId('pending'),
    role: 'user',
    user_name: 'Stellacode',
    message_time: new Date().toISOString(),
    preview: text,
    has_token_usage: false,
    pending: true,
    conversationKey: localKey
  };
  if (!isControlCommand) {
    state.optimisticMessages.push(optimistic);
    renderMessages({ stickToBottom: true });
  }
  try {
    await postConversationMessage(serverId, conversationId, text);
    markOptimisticMessageSent(optimistic.id);
    renderMessages({ stickToBottom: true });
    if (!state.websocket || state.websocket.readyState !== WebSocket.OPEN) {
      refreshConversation().finally(() => pollActiveConversation());
    }
  } catch (error) {
    try {
      if (!needsModel) {
        throw error;
      }
      await recoverModelAndSend(serverId, conversationId, text, error);
      removeOptimisticMessage(optimistic.id);
      renderMessages({ stickToBottom: true });
      await refreshConversation();
      if (!state.websocket || state.websocket.readyState !== WebSocket.OPEN) {
        pollActiveConversation();
      }
    } catch (recoveryError) {
      removeOptimisticMessage(optimistic.id);
      elements.composerInput.value = text;
      autosizeComposer();
      renderMessages();
      showToast(recoveryError.message);
    }
  } finally {
    elements.sendButton.disabled = false;
  }
}

function pollActiveConversation() {
  clearActivePoll();
  const key = selectedKey();
  const delays = [800, 1200, 1800, 2500, 3500, 5000, 7000, 9000, 12000, 15000, 18000, 22000];
  let index = 0;
  const tick = async () => {
    if (!state.selected || selectedKey() !== key || index >= delays.length) {
      clearActivePoll();
      return;
    }
    const before = state.messagesSignature;
    await refreshConversation();
    if (selectedKey() !== key) {
      clearActivePoll();
      return;
    }
    if (state.messagesSignature !== before) {
      index = Math.max(0, index - 2);
    } else {
      index += 1;
    }
    state.activePoll = setTimeout(tick, delays[Math.min(index, delays.length - 1)]);
  };
  state.activePoll = setTimeout(tick, delays[0]);
}

async function renameConversation(serverId, conversation) {
  const current = displayConversationName(serverId, conversation);
  return new Promise((resolve) => {
    openModal(`
      <div class="modal-card small">
        <div class="modal-head">
          <h2>重命名会话</h2>
          <button class="icon-button" type="button" data-close-modal>×</button>
        </div>
        <label class="field-label modal-field">
          会话名称
          <input id="renameConversationInput" type="text" value="${escapeHtml(current)}" />
        </label>
        <div class="modal-actions">
          <button id="renameConversationConfirm" class="primary-button" type="button">确定</button>
        </div>
      </div>
    `);
    const input = $('#renameConversationInput');
    if (input) {
      input.select();
    }
    async function doRename() {
      const name = input ? input.value : current;
      const nickname = name.trim() || conversation.conversation_id;
      const key = conversationKey(serverId, conversation.conversation_id);
      closeModal();
      try {
        const response = await api(serverId, `/api/conversations/${conversation.conversation_id}`, {
          method: 'PATCH',
          body: { nickname }
        });
        const updated = response.data?.conversation;
        const list = state.conversations.get(serverId) || [];
        state.conversations.set(
          serverId,
          list.map((item) => (item.conversation_id === conversation.conversation_id ? { ...item, ...updated } : item))
        );
        delete state.settings.conversationNames[key];
        saveSettingsSoon();
        renderSidebar();
        renderHeader();
      } catch (error) {
        showToast(error.message);
      }
      resolve();
    }
    $('#renameConversationConfirm')?.addEventListener('click', doRename);
    input?.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && !event.isComposing) {
        event.preventDefault();
        doRename();
      }
    });
  });
}

function visibleConversationRows() {
  const rows = [];
  for (const server of getServers()) {
    for (const conversation of visibleConversations(server.id)) {
      rows.push({ server, conversation });
    }
  }
  rows.sort((left, right) =>
    left.conversation.conversation_id.localeCompare(right.conversation.conversation_id, undefined, {
      numeric: true
    })
  );
  return rows;
}

function hideConversation(serverId, conversation) {
  const label = displayConversationName(serverId, conversation);
  const confirmed = window.confirm(`从 Stellacode 左侧列表删除这个 Conversation？\n\n${label}\n\n后端 workdir 文件不会被移除。`);
  if (!confirmed) {
    return;
  }
  const key = conversationKey(serverId, conversation.conversation_id);
  ensureLocalSettings();
  state.settings.hiddenConversations[key] = true;
  delete state.settings.conversationNames[key];
  state.statuses.delete(key);
  state.optimisticMessages = state.optimisticMessages.filter((message) => message.conversationKey !== key);
  saveSettingsSoon();
  if (state.selected?.serverId === serverId && state.selected?.conversationId === conversation.conversation_id) {
    const next = visibleConversationRows().find(
      (row) => !(row.server.id === serverId && row.conversation.conversation_id === conversation.conversation_id)
    );
    state.selected = null;
    state.messages = [];
    state.messagesSignature = '';
    if (next) {
      selectConversation(next.server.id, next.conversation.conversation_id);
    } else {
      renderHeader();
      renderMessages();
      renderContext();
    }
  }
  renderSidebar();
}

function closeConversationMenu() {
  state.conversationMenu?.remove();
  state.conversationMenu = null;
}

function showConversationMenu(event, serverId, conversation) {
  closeConversationMenu();
  const menu = document.createElement('div');
  menu.className = 'conversation-menu';
  menu.setAttribute('role', 'menu');
  menu.innerHTML = `
    <button type="button" role="menuitem" data-action="rename">重命名</button>
    <button type="button" role="menuitem" class="danger" data-action="delete">删除</button>
  `;
  document.body.append(menu);
  const rect = menu.getBoundingClientRect();
  const left = Math.min(Math.max(8, event.clientX), window.innerWidth - rect.width - 8);
  const top = Math.min(Math.max(8, event.clientY), window.innerHeight - rect.height - 8);
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
  menu.querySelector('[data-action="rename"]')?.addEventListener('click', () => {
    closeConversationMenu();
    renameConversation(serverId, conversation).catch((error) => showToast(error.message));
  });
  menu.querySelector('[data-action="delete"]')?.addEventListener('click', () => {
    closeConversationMenu();
    hideConversation(serverId, conversation);
  });
  menu.addEventListener('click', (menuEvent) => menuEvent.stopPropagation());
  state.conversationMenu = menu;
}

function renderSidebar() {
  const fragment = document.createDocumentFragment();
  const rows = visibleConversationRows();

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
      <span class="conversation-age">${escapeHtml(displayConversationModel(conversation, status))}</span>
    `;
    row.addEventListener('click', () => selectConversation(server.id, conversation.conversation_id));
    row.addEventListener('dblclick', () => renameConversation(server.id, conversation).catch((error) => showToast(error.message)));
    row.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      event.stopPropagation();
      showConversationMenu(event, server.id, conversation);
    });
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
  elements.conversationSubtitle.textContent = conversationHeaderSubtitle(server, conversation, status);
  elements.composerHint.textContent = isRemoteStatus(status)
    ? `Remote: ${status.remote} · ${status.workspace}${state.sessionActivity ? ` · ${state.sessionActivity}` : ''}`
    : state.lastRefreshAt
      ? `本地模式 · ${formatRelative(state.lastRefreshAt)}前刷新${state.sessionActivity ? ` · ${state.sessionActivity}` : ''}`
      : '本地模式';
  elements.composerModePill.textContent = isRemoteStatus(status) ? 'Remote' : '本地';
}

function roleClass(role) {
  return String(role || '').toLowerCase() === 'user' ? 'user' : 'assistant';
}

function renderMessageAttachments(message, detail, options = {}) {
  if (options.hideSynthetic && isSyntheticMediaContextMessage(message)) {
    return '';
  }
  const attachments = detail?.attachments || message.attachments || [];
  const errors = detail?.attachment_errors || [];
  if (attachments.length === 0 && errors.length === 0) {
    if (Number(message.attachment_count || 0) > 0 || message.has_attachment_errors) {
      return '<div class="message-attachments muted">正在加载附件...</div>';
    }
    return '';
  }
  const key = selectedKey();
  const inlineAttachmentIndexes = messageInlineAttachmentIndexes(message, detail);
  const items = attachments
    .filter((attachment, index) => !inlineAttachmentIndexes.has(index) && !inlineAttachmentIndexes.has(Number(attachment?.index)))
    .map((attachment) => renderMessageAttachmentItem(attachment, key))
    .join('');
  const errorItems = errors.map((error) => `<div class="message-attachment-error">${escapeHtml(error)}</div>`).join('');
  if (!items && !errorItems) {
    return '';
  }
  return `<div class="message-attachments">${items}${errorItems}</div>`;
}

function renderMessageAttachmentItem(attachment, key = selectedKey()) {
  if (!attachment) {
    return '';
  }
  const name = attachment.name || fileNameFromPath(attachment.path || '') || 'attachment';
  const path = normalizeWorkspacePath(attachment.path || '');
  if (isImageAttachment(attachment) && path) {
    const preview = workspaceFilePreview(key, path);
    const loading = workspaceFileIsLoading(key, path);
    const thumbnailUrl = attachmentThumbnailUrl(attachment);
    if (thumbnailUrl) {
      return `
        <button class="message-attachment image" type="button" data-workspace-file="${escapeHtml(path)}">
          <img src="${escapeHtml(thumbnailUrl)}" alt="${escapeHtml(name)}" loading="lazy" />
          <span>${escapeHtml(name)}</span>
        </button>
      `;
    }
    if (preview?.encoding === 'base64') {
      return `
        <button class="message-attachment image" type="button" data-workspace-file="${escapeHtml(path)}">
          <img src="data:${escapeHtml(attachmentMimeType(attachment))};base64,${escapeHtml(preview.data || '')}" alt="${escapeHtml(name)}" loading="lazy" />
          <span>${escapeHtml(name)}</span>
        </button>
      `;
    }
    if (preview?.encoding === 'utf8' && fileExtension(name) === 'svg') {
      return `
        <button class="message-attachment image" type="button" data-workspace-file="${escapeHtml(path)}">
          <img src="data:image/svg+xml;charset=utf-8,${encodeURIComponent(preview.data || '')}" alt="${escapeHtml(name)}" loading="lazy" />
          <span>${escapeHtml(name)}</span>
        </button>
      `;
    }
    return `
      <button class="message-attachment image loading" type="button" data-workspace-file="${escapeHtml(path)}">
        <span>${escapeHtml(loading ? '正在加载图片...' : name)}</span>
      </button>
    `;
  }
  return `
    <button class="message-attachment file" type="button" data-workspace-file="${escapeHtml(path)}">
      <span class="attachment-file-icon">${workspaceKindIcon({ kind: 'file', name, path })}</span>
      <span>${escapeHtml(name)}</span>
      <small>${escapeHtml(formatBytes(attachment.size_bytes))}</small>
    </button>
  `;
}

function hydrateMessageAttachments() {
  const key = selectedKey();
  if (!key) {
    return;
  }
  const targets = state.messages.filter(
    (message) =>
      (Number(message.attachment_count || 0) > 0 || message.has_attachment_errors) &&
      (!Array.isArray(message.attachments) || message.attachments.length === 0 || message.has_attachment_errors) &&
      !state.messageDetails.has(message.id) &&
      !state.messageDetailsLoading.has(message.id)
  );
  if (targets.length > 0) {
    Promise.allSettled(targets.map((message) => fetchMessageDetail(message.id))).then(() => {
      if (selectedKey() !== key) {
        return;
      }
      renderMessages();
      hydrateMessageAttachments();
    });
  }
  for (const message of state.messages) {
    const detail = state.messageDetails.get(message.id);
    for (const attachment of detail?.attachments || message.attachments || []) {
      const path = normalizeWorkspacePath(attachment.path || '');
      if (
        path &&
        isImageAttachment(attachment) &&
        !attachmentThumbnailUrl(attachment) &&
        !workspaceFilePreview(key, path) &&
        !workspaceFileIsLoading(key, path) &&
        !workspaceFileError(key, path)
      ) {
        const limitBytes = Math.min(Math.max(Number(attachment.size_bytes || 0), 65536), 8 * 1024 * 1024);
        fetchWorkspaceFile(path, {
          renderContextOnChange: false,
          renderMessagesOnChange: true,
          limitBytes,
          apiPath: attachment.url || ''
        }).catch(() => {
          if (selectedKey() === key) {
            renderMessages();
          }
        });
      }
    }
  }
}

function hydrateMessageDetails() {
  const key = selectedKey();
  if (!key) {
    return;
  }
  const targets = state.messages
    .filter(
      (message) =>
        messageNeedsFullDetail(message) &&
        !state.messageDetails.has(message.id) &&
        !state.messageDetailsLoading.has(message.id)
    )
    .slice(0, 24);
  if (targets.length === 0) {
    return;
  }
  mapLimit(targets, 3, (message) => fetchMessageDetail(message.id)).then(() => {
    if (selectedKey() !== key) {
      return;
    }
    renderMessages();
    hydrateMessageDetails();
  });
}

function executionGroupId(messages) {
  return messages.map((message) => safeText(message.id || message.index)).filter(Boolean).join(':');
}

function executionGroupSummary(messages, nextMessage) {
  const labels = [];
  for (const message of messages) {
    for (const item of messageItems(message)) {
      if ((item.type === 'tool_call' || item.type === 'tool_result') && item.tool_name) {
        labels.push(item.tool_name);
      }
    }
  }
  const uniqueLabels = [...new Set(labels)].slice(0, 3);
  const labelText = uniqueLabels.length ? ` · ${uniqueLabels.join(', ')}` : '';
  const elapsed = formatElapsed(messages[0]?.message_time, nextMessage?.message_time);
  return `${nextMessage ? '已处理' : '正在处理'}${elapsed ? ` ${elapsed}` : ''}${labelText}`;
}

function renderExecutionMessageDetail(message) {
  const detail = state.messageDetails.get(message.id);
  const bodyHtml = renderStructuredMessageContent(message, detail, {
    hideFiles: false,
    plainSyntheticSummary: false
  });
  const attachmentsHtml = messageHasItemType(message, ['file']) ? '' : renderMessageAttachments(message, detail);
  return `
    <section class="execution-detail-item">
      <div class="execution-detail-meta">${escapeHtml(message.user_name || message.role || 'assistant')}</div>
      <div class="execution-detail-body">${bodyHtml}${attachmentsHtml}${renderTokenUsageSummary(message)}</div>
    </section>
  `;
}

function createExecutionGroup(messages, nextMessage) {
  const groupId = executionGroupId(messages);
  const expanded = state.expandedExecutionGroups.has(groupId);
  const element = document.createElement('section');
  element.className = `execution-group${expanded ? ' expanded' : ''}`;
  element.dataset.executionGroupId = groupId;
  const toolCount = messages.reduce(
    (count, message) => count + messageItems(message).filter((item) => item.type === 'tool_call' || item.type === 'tool_result').length,
    0
  );
  element.innerHTML = `
    <button class="execution-summary" type="button" aria-expanded="${expanded ? 'true' : 'false'}">
      <span>${escapeHtml(executionGroupSummary(messages, nextMessage))}</span>
      <span class="execution-count">${toolCount ? `${toolCount} 项` : `${messages.length} 条`}</span>
      <span class="execution-chevron">${icons.chevronRight}</span>
    </button>
    ${expanded ? `<div class="execution-details">${messages.map(renderExecutionMessageDetail).join('')}</div>` : ''}
  `;
  element.querySelector('.execution-summary')?.addEventListener('click', () => {
    const current = elements.messageList.querySelector(`[data-execution-group-id="${CSS.escape(groupId)}"]`);
    if (state.expandedExecutionGroups.has(groupId)) {
      state.expandedExecutionGroups.delete(groupId);
    } else {
      state.expandedExecutionGroups.add(groupId);
    }
    const replacement = createExecutionGroup(messages, nextMessage);
    current?.replaceWith(replacement);
    hydrateMessageAttachments();
  });
  element.querySelectorAll('[data-workspace-file]').forEach((button) => {
    button.addEventListener('click', (event) => {
      event.stopPropagation();
      selectWorkspaceFile(button.dataset.workspaceFile || '').catch((error) => showToast(error.message));
    });
  });
  element.querySelectorAll('[data-token-usage]').forEach((button) => {
    button.addEventListener('click', (event) => {
      event.stopPropagation();
      const message = messages.find((item) => item.id === button.dataset.tokenMessageId);
      if (message) {
        showTokenUsagePopover(button, message);
      }
    });
  });
  return element;
}

function createAssistantFinalDivider() {
  const divider = document.createElement('div');
  divider.className = 'assistant-final-divider';
  divider.innerHTML = '<span></span>';
  return divider;
}

function renderSessionProgressCard() {
  const progress = state.sessionProgress;
  if (!progress || progress.conversationKey !== selectedKey()) {
    return null;
  }
  const parsed = progress.parsed || {};
  const element = document.createElement('section');
  element.className = 'session-progress-card';
  const runningTool = runningToolForProgress(parsed);
  const toolHtml = runningTool
    ? `<div class="session-progress-tool">${renderToolCard('call', runningTool.name, formatToolPayload(runningTool.payload))}</div>`
    : '';
  const plan = Array.isArray(parsed.plan) ? parsed.plan : [];
  const planHtml = plan.length
    ? `<div class="session-progress-plan">${plan
        .map((item) => {
          const markerClass = item.marker === '☑' ? 'done' : item.marker === '◐' ? 'active' : '';
          return `<div class="session-progress-step ${markerClass}">
            <span>${escapeHtml(item.marker || '•')}</span>
            <span>${escapeHtml(item.text)}</span>
          </div>`;
        })
        .join('')}</div>`
    : '';
  element.innerHTML = `
    <div class="session-progress-head">
      <span class="session-progress-dot"></span>
      <span>${escapeHtml(parsed.title || '正在执行')}</span>
      ${parsed.model ? `<code>${escapeHtml(parsed.model)}</code>` : ''}
    </div>
    ${runningTool ? '' : parsed.phase ? `<div class="session-progress-phase">${escapeHtml(parsed.phase)}</div>` : ''}
    ${toolHtml}
    ${planHtml}
  `;
  return element;
}

function renderChannelEventCard(event) {
  const element = document.createElement('section');
  element.className = `channel-event ${event.severity || 'error'}`;
  const detailText = event.detail ? JSON.stringify(event.detail, null, 2) : '';
  const detailHtml = detailText
    ? `<details class="channel-event-detail">
        <summary>详情</summary>
        <pre><code>${escapeHtml(detailText)}</code></pre>
      </details>`
    : '';
  element.innerHTML = `
    <div class="channel-event-head">
      <span>${escapeHtml(event.severity === 'warning' ? '警告' : event.severity === 'info' ? '提示' : '错误')}</span>
      <code>${escapeHtml(event.code || event.scope || 'error')}</code>
    </div>
    <div class="channel-event-message">${escapeHtml(event.message)}</div>
    ${event.suggestedAction ? `<div class="channel-event-action">${escapeHtml(event.suggestedAction)}</div>` : ''}
    ${detailHtml}
  `;
  return element;
}

function visibleChannelEvents() {
  const key = selectedKey();
  if (!key) {
    return [];
  }
  return state.channelEvents.filter((event) => event.conversationKey === key);
}

function renderRuntimeMetadataDots(message) {
  const items = Array.isArray(message.runtimeMetadata) ? message.runtimeMetadata : [];
  if (items.length === 0) {
    return '';
  }
  return `
    <div class="message-metadata-dots" aria-label="消息上下文">
      ${items
        .map(
          (item, index) => `
            <button
              class="message-metadata-dot ${escapeHtml(item.className || 'metadata')}"
              type="button"
              title="${escapeHtml(item.label)}"
              aria-label="${escapeHtml(item.label)}"
              data-metadata-index="${index}"
            ></button>
          `
        )
        .join('')}
    </div>
  `;
}

function showRuntimeMetadataPopover(anchor, message, metadataIndex) {
  closeMetadataPopover();
  closeTokenUsagePopover();
  const items = Array.isArray(message.runtimeMetadata) ? message.runtimeMetadata : [];
  const item = items[metadataIndex];
  if (!item) {
    return;
  }
  const popover = document.createElement('div');
  popover.className = 'metadata-popover';
  popover.innerHTML = `
    <div class="metadata-popover-head">
      <span class="message-metadata-dot ${escapeHtml(item.className || 'metadata')}"></span>
      <strong>${escapeHtml(item.label)}</strong>
    </div>
    <pre>${escapeHtml(item.text)}</pre>
  `;
  document.body.append(popover);
  const anchorRect = anchor.getBoundingClientRect();
  const popoverRect = popover.getBoundingClientRect();
  const left = Math.min(Math.max(12, anchorRect.left), window.innerWidth - popoverRect.width - 12);
  const top = Math.min(anchorRect.bottom + 8, window.innerHeight - popoverRect.height - 12);
  popover.style.left = `${left}px`;
  popover.style.top = `${Math.max(12, top)}px`;
  state.metadataPopover = popover;
  window.setTimeout(() => {
    document.addEventListener('click', closeMetadataPopover, { once: true });
  }, 0);
}

function closeMetadataPopover() {
  state.metadataPopover?.remove();
  state.metadataPopover = null;
}

function closeTokenUsagePopover() {
  state.tokenUsagePopover?.remove();
  state.tokenUsagePopover = null;
}

function renderTokenUsageSummary(message) {
  if (roleClass(message.role) !== 'assistant') {
    return '';
  }
  const usage = messageTokenUsage(message);
  const active = usage.totalTokens > 0 || usage.cost > 0;
  return `
    <button class="message-token-usage${active ? ' has-usage' : ''}" type="button" data-token-usage data-token-message-id="${escapeHtml(message.id || '')}" title="Token Usage">
      <span class="token-usage-dot" aria-hidden="true"></span>
      <span>${escapeHtml(formatCompactNumber(usage.totalTokens))} tokens</span>
    </button>
  `;
}

function renderTokenUsagePopoverBody(message) {
  const usage = messageTokenUsage(message);
  const loading = state.messageDetailsLoading.has(message.id);
  return `
    <div class="token-usage-popover-head">
      <strong>Token Usage</strong>
      ${loading ? '<span>正在载入...</span>' : ''}
    </div>
    <div class="token-usage-grid">
      <span>Cache Read</span><strong>${escapeHtml(formatCompactNumber(usage.cacheRead))}</strong>
      <span>Cache Write</span><strong>${escapeHtml(formatCompactNumber(usage.cacheWrite))}</strong>
      <span>Input</span><strong>${escapeHtml(formatCompactNumber(usage.input))}</strong>
      <span>Output</span><strong>${escapeHtml(formatCompactNumber(usage.output))}</strong>
      <span>Total</span><strong>${escapeHtml(formatCompactNumber(usage.totalTokens))}</strong>
      <span>Cost</span><strong>${escapeHtml(formatCost(usage.cost))}</strong>
    </div>
  `;
}

function positionPopover(popover, anchor) {
  const anchorRect = anchor.getBoundingClientRect();
  const popoverRect = popover.getBoundingClientRect();
  const left = Math.min(Math.max(12, anchorRect.right - popoverRect.width), window.innerWidth - popoverRect.width - 12);
  const top = Math.min(anchorRect.bottom + 8, window.innerHeight - popoverRect.height - 12);
  popover.style.left = `${left}px`;
  popover.style.top = `${Math.max(12, top)}px`;
}

function showTokenUsagePopover(anchor, message) {
  closeMetadataPopover();
  closeTokenUsagePopover();
  const popover = document.createElement('div');
  popover.className = 'token-usage-popover';
  popover.dataset.messageId = message.id;
  popover.innerHTML = renderTokenUsagePopoverBody(message);
  document.body.append(popover);
  positionPopover(popover, anchor);
  state.tokenUsagePopover = popover;
  window.setTimeout(() => {
    document.addEventListener('click', closeTokenUsagePopover, { once: true });
  }, 0);
  if (!state.messageDetails.has(message.id) && !state.messageDetailsLoading.has(message.id)) {
    fetchMessageDetail(message.id)
      .then(() => {
        if (state.tokenUsagePopover?.dataset.messageId === message.id) {
          state.tokenUsagePopover.innerHTML = renderTokenUsagePopoverBody(message);
          positionPopover(state.tokenUsagePopover, anchor);
        }
      })
      .catch((error) => showToast(error.message));
  }
}

function createMessageArticle(message, index, messages, options = {}) {
  const expanded = state.expandedMessages.has(message.id);
  const detail = state.messageDetails.get(message.id);
  const article = document.createElement('article');
  const previous = messages[index - 1];
  const sameSideAsPrevious = !options.forceSeparate && previous && roleClass(previous.role) === roleClass(message.role);
  const executionFrame = isExecutionMessage(message);
  article.className = `message ${roleClass(message.role)}${expanded ? ' expanded' : ''}${
    message.pending ? ' pending' : ''
  }${sameSideAsPrevious ? ' compact' : ''}${executionFrame ? ' execution-frame' : ''}`;
  article.dataset.messageId = message.id;
  const syntheticCollapsed = isSyntheticMediaContextMessage(message) && !expanded;
  const hasStructuredFileItems = (detail?.items || message.items || []).some((item) => item.type === 'file');
  const bodyHtml = renderStructuredMessageContent(message, detail, {
    hideFiles: syntheticCollapsed,
    plainSyntheticSummary: syntheticCollapsed
  });
  const attachmentsHtml = hasStructuredFileItems || syntheticCollapsed ? '' : renderMessageAttachments(message, detail);
  const actionsHtml = message.pending
    ? '<span class="message-actions"><span>正在发送...</span></span>'
    : message.localOnly
      ? '<span class="message-actions"><span>等待回应</span></span>'
      : '';
  article.innerHTML = `
    <div class="message-bubble" role="button" tabindex="0">
      ${renderRuntimeMetadataDots(message)}
      <span class="message-meta">${escapeHtml(message.user_name || message.role || 'assistant')} ${
        message.message_time ? `· ${escapeHtml(formatRelative(message.message_time))}` : ''
      }</span>
      <div class="message-text">${bodyHtml}</div>
      ${attachmentsHtml}
      ${actionsHtml}
      ${renderTokenUsageSummary(message)}
    </div>
  `;
  bindMessageArticle(article, message);
  return article;
}

function bindMessageArticle(article, message) {
  const bubble = article.querySelector('.message-bubble');
  if (!bubble || message.pending || message.localOnly) {
    return;
  }
  bubble.querySelectorAll('.tool-card').forEach((card) => {
    card.addEventListener('click', (event) => {
      event.stopPropagation();
    });
    card.addEventListener('keydown', (event) => {
      event.stopPropagation();
    });
    card.addEventListener('toggle', (event) => {
      event.stopPropagation();
    });
  });
  bubble.querySelectorAll('[data-workspace-file]').forEach((button) => {
    button.addEventListener('click', (event) => {
      event.stopPropagation();
      selectWorkspaceFile(button.dataset.workspaceFile || '').catch((error) => showToast(error.message));
    });
  });
  bubble.querySelectorAll('[data-metadata-index]').forEach((button) => {
    button.addEventListener('click', (event) => {
      event.stopPropagation();
      showRuntimeMetadataPopover(button, message, Number(button.dataset.metadataIndex || 0));
    });
  });
  bubble.querySelector('[data-token-usage]')?.addEventListener('click', (event) => {
    event.stopPropagation();
    showTokenUsagePopover(event.currentTarget, message);
  });
  bubble.addEventListener('click', (event) => {
    if (isSyntheticMediaContextMessage(message)) {
      toggleMessage(message.id);
    }
  });
  bubble.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      if (isSyntheticMediaContextMessage(message)) {
        toggleMessage(message.id);
      }
    }
  });
}

function updateMessageArticle(messageId) {
  const messages = visibleMessages();
  const index = messages.findIndex((message) => message.id === messageId);
  if (index < 0) {
    return;
  }
  const current = elements.messageList.querySelector(`[data-message-id="${CSS.escape(messageId)}"]`);
  if (!current) {
    return;
  }
  current.replaceWith(createMessageArticle(messages[index], index, messages));
}

function renderMessages(options = {}) {
  const { stickToBottom = false, preserveScroll = false } = options;
  const wasNearBottom = isMessageListNearBottom();
  if (!state.selected) {
    elements.messageList.innerHTML = `
      <div class="empty-state">
        <div class="empty-title">连接到 Stellaclaw</div>
        <div class="empty-copy">先在设置里添加服务器，然后创建或选择一个 Conversation。</div>
      </div>
    `;
    return;
  }
  const messages = visibleMessages();
  const channelEvents = visibleChannelEvents();
  const progressCard = renderSessionProgressCard();
  if (messages.length === 0 && channelEvents.length === 0 && !progressCard) {
    const status = selectedStatus();
    const conversation = currentConversation();
    const pendingModel = status?.model_selection_pending || conversation?.model_selection_pending;
    elements.messageList.innerHTML = `
      <div class="empty-state">
        <div class="empty-title">新的 Conversation</div>
        <div class="empty-copy">${
          pendingModel ? '当前后端需要先发送 /model <模型别名> 选择可用模型。' : '可以直接开始输入，也可以先发送 /model 选择模型。'
        }</div>
      </div>
    `;
    return;
  }
  const fragment = document.createDocumentFragment();
  if (messagePageHasOlder()) {
    const loader = document.createElement('div');
    loader.className = 'older-message-loader';
    loader.innerHTML = `
      <button class="tiny-button" type="button" ${state.loadingOlderMessages ? 'disabled' : ''}>
        ${state.loadingOlderMessages ? '正在加载...' : '加载更早消息'}
      </button>
    `;
    loader.querySelector('button')?.addEventListener('click', () => loadOlderMessages());
    fragment.append(loader);
  }
  let forceSeparateNext = false;
  for (let index = 0; index < messages.length; index += 1) {
    const message = messages[index];
    if (isExecutionMessage(message)) {
      const group = [];
      let cursor = index;
      while (cursor < messages.length && isExecutionMessage(messages[cursor])) {
        group.push(messages[cursor]);
        cursor += 1;
      }
      const nextMessage = messages[cursor];
      if (isFinalAssistantMessage(nextMessage)) {
        fragment.append(createExecutionGroup(group, nextMessage));
        forceSeparateNext = true;
      } else {
        for (let groupIndex = 0; groupIndex < group.length; groupIndex += 1) {
          fragment.append(createMessageArticle(group[groupIndex], index + groupIndex, messages));
        }
      }
      index = cursor - 1;
      continue;
    }
    fragment.append(createMessageArticle(message, index, messages, { forceSeparate: forceSeparateNext }));
    forceSeparateNext = false;
  }
  if (progressCard) {
    fragment.append(progressCard);
  }
  for (const event of channelEvents) {
    fragment.append(renderChannelEventCard(event));
  }
  elements.messageList.replaceChildren(fragment);
  const shouldKeepBottom = !preserveScroll && (stickToBottom || wasNearBottom || messages.length !== state.messages.length);
  if (shouldKeepBottom) {
    scrollMessagesToBottom();
  }
  if (shouldKeepBottom) {
    elements.messageList.querySelectorAll('img').forEach((image) => {
      if (!image.complete) {
        image.addEventListener('load', () => scrollMessagesToBottom(), { once: true });
        image.addEventListener('error', () => scrollMessagesToBottom(), { once: true });
      }
    });
  }
  hydrateMessageAttachments();
}

function isMessageListNearBottom(threshold = 120) {
  const list = elements.messageList;
  if (!list) {
    return true;
  }
  return list.scrollHeight - list.scrollTop - list.clientHeight <= threshold;
}

function scrollMessagesToBottom() {
  const list = elements.messageList;
  if (!list) {
    return;
  }
  const apply = () => {
    list.scrollTop = list.scrollHeight;
  };
  apply();
  requestAnimationFrame(() => {
    apply();
    requestAnimationFrame(apply);
  });
  window.setTimeout(apply, 80);
  window.setTimeout(apply, 240);
}

function updateMessageSelectionState() {
  elements.messageList.querySelectorAll('[data-message-id]').forEach((article) => {
    const active = article.dataset.messageId === state.activePreviewMessageId;
    article.classList.toggle('active-preview', active);
  });
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
  return visibleConversations(state.selected.serverId).find(
    (item) => item.conversation_id === state.selected.conversationId
  );
}

function preferredModelFor(serverId) {
  return modelCandidatesFor(serverId)[0] || '';
}

function modelAlias(model) {
  return safeText(model?.alias || model?.name).trim();
}

async function fetchModels(serverId) {
  const response = await api(serverId, '/api/models');
  return response.data?.models || [];
}

function renderContext() {
  if (state.activeContextTab === 'detail') {
    state.activeContextTab = 'overview';
  }
  if (state.activeContextTab === 'file') {
    state.activeContextTab = 'overview';
  }
  if (state.fileBarOpen) {
    state.contextCollapsed = true;
  }
  applyLayoutSettings();
  document.body.classList.toggle('context-collapsed', state.contextCollapsed);
  document.body.classList.toggle('file-bar-open', state.fileBarOpen);
  document.body.classList.toggle('terminal-open', state.terminalOpen);
  elements.toggleContextButton.innerHTML = state.contextCollapsed ? icons.panelOpen : icons.panelClose;
  elements.toggleContextButton.title = state.contextCollapsed ? '打开详情' : '收起详情';
  elements.toggleFileButton.classList.toggle('active', state.fileBarOpen);
  elements.toggleFileButton.title = state.fileBarOpen ? '关闭文件' : '打开文件';
  elements.toggleTerminalButton.classList.toggle('active', state.terminalOpen);
  elements.toggleTerminalButton.title = state.terminalOpen ? '关闭终端' : '打开终端';
  document.querySelectorAll('.context-tab').forEach((tab) => {
    tab.classList.toggle('active', tab.dataset.contextTab === state.activeContextTab);
  });
  if (!elements.contextContent) {
    return;
  }
  renderOverviewContext();
  if (state.fileBarOpen) {
    renderFilesContext();
  } else if (elements.fileContent) {
    elements.fileContent.innerHTML = '';
  }
  if (state.terminalOpen) {
    renderTerminalContext();
  } else if (elements.terminalContent) {
    disposeXtermSessions();
    elements.terminalContent.innerHTML = '';
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
      <div class="inspector-title">Usage</div>
      ${usageBar('Cache Read', usage.cacheRead, usage.totalTokens)}
      ${usageBar('Cache Write', usage.cacheWrite, usage.totalTokens)}
      ${usageBar('Input', usage.input, usage.totalTokens)}
      ${usageBar('Output', usage.output, usage.totalTokens)}
    </section>
  `;
}

function renderFilesContext() {
  const target = elements.fileContent;
  if (!target) {
    return;
  }
  if (!state.selected) {
    target.innerHTML = `
      <div class="context-empty">
        <strong>没有打开的工作区</strong>
        <span>选择一个 Conversation 后查看文件。</span>
      </div>
    `;
    return;
  }
  const previewHtml = selectedFileTabs().length > 0
    ? `<section class="file-preview-stack">${renderFilePreviewShell()}</section>`
    : '';
  target.innerHTML = `
    <div class="file-panel-layout${previewHtml ? ' has-preview' : ''}">
      <div class="file-tree-pane">${renderWorkspaceCard(selectedStatus())}</div>
      ${previewHtml}
    </div>
  `;
  bindWorkspaceActions();
  if (previewHtml) {
    bindEditorActions(target);
  }
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

function workspaceKindIcon(entry) {
  const name = safeText(entry?.name).toLowerCase();
  if (entry?.kind === 'symlink') {
    return `<span class="file-badge file-badge-icon symlink">${icons.symlink}</span>`;
  }
  if (name.endsWith('.html') || name.endsWith('.htm')) {
    return '<span class="file-badge html">#</span>';
  }
  if (name.endsWith('.js') || name.endsWith('.mjs') || name.endsWith('.cjs')) {
    return '<span class="file-badge js">JS</span>';
  }
  if (name.endsWith('.css')) {
    return '<span class="file-badge css">CSS</span>';
  }
  if (name.endsWith('.md')) {
    return '<span class="file-badge md">M</span>';
  }
  if (name.endsWith('.json')) {
    return '<span class="file-badge json">{}</span>';
  }
  if (name.endsWith('.sh') || name.endsWith('.bash') || name.endsWith('.zsh')) {
    return '<span class="file-badge shell">$</span>';
  }
  if (name.endsWith('.png') || name.endsWith('.jpg') || name.endsWith('.jpeg') || name.endsWith('.gif') || name.endsWith('.webp')) {
    return '<span class="file-badge image">◇</span>';
  }
  if (name.endsWith('.pdf')) {
    return '<span class="file-badge pdf">P</span>';
  }
  return `<span class="file-badge file-badge-icon file">${icons.file}</span>`;
}

function sortedWorkspaceEntries(entries) {
  return [...(entries || [])].sort((left, right) => {
    const leftDir = left.kind === 'directory';
    const rightDir = right.kind === 'directory';
    if (leftDir !== rightDir) {
      return leftDir ? -1 : 1;
    }
    return safeText(left.name).localeCompare(safeText(right.name), undefined, { numeric: true });
  });
}

function workspaceEntryMatches(entry) {
  const filter = state.workspaceFilter.trim().toLowerCase();
  if (entry.hidden && !filter.includes('.')) {
    return false;
  }
  if (!filter) {
    return true;
  }
  return `${entry.name} ${entry.path}`.toLowerCase().includes(filter);
}

function renderWorkspaceTree(key, path = '', depth = 0) {
  const indent = depth * 34;
  const listing = workspaceListing(key, path);
  const loading = workspaceIsLoading(key, path);
  const error = workspaceError(key, path);
  if (error) {
    return `<div class="workspace-tree-note error" style="--tree-indent: ${indent}px" data-depth="${depth}">${escapeHtml(error)}</div>`;
  }
  if (!listing) {
    return loading
      ? `<div class="workspace-tree-note" style="--tree-indent: ${indent}px" data-depth="${depth}">正在加载...</div>`
      : `<div class="workspace-tree-note" style="--tree-indent: ${indent}px" data-depth="${depth}">尚未读取这个目录。</div>`;
  }
  const entries = sortedWorkspaceEntries(listing.entries).filter(workspaceEntryMatches);
  if (entries.length === 0) {
    return `<div class="workspace-tree-note" style="--tree-indent: ${indent}px" data-depth="${depth}">${state.workspaceFilter ? '没有匹配文件' : '空目录'}</div>`;
  }
  const expanded = workspaceExpandedSet(key);
  const activePath = currentWorkspacePath();
  return entries
    .map((entry) => {
      const entryPath = normalizeWorkspacePath(entry.path);
      const isDirectory = entry.kind === 'directory';
      const isExpanded = isDirectory && expanded.has(entryPath);
      const isSelected = entryPath === activePath;
      const isActiveFile = !isDirectory && entryPath === state.activePreviewFilePath;
      const metaParts = [entry.kind === 'file' ? formatBytes(entry.size_bytes) : '', entry.readonly ? 'readonly' : ''].filter(Boolean);
      const meta = metaParts.length > 0 ? `<span class="workspace-tree-meta">${escapeHtml(metaParts.join(' · '))}</span>` : '';
      const indentStyle = `--tree-indent: ${indent}px`;
      const row = isDirectory
        ? `
          <button class="workspace-tree-row directory${isSelected ? ' selected' : ''}" type="button" style="${indentStyle}" data-depth="${depth}" data-workspace-toggle="${escapeHtml(entryPath)}" aria-expanded="${isExpanded ? 'true' : 'false'}">
            <span class="workspace-tree-guide"></span>
            <span class="workspace-chevron">${isExpanded ? icons.chevronDown : icons.chevronRight}</span>
            <span class="workspace-file-icon directory-spacer">${icons.folder}</span>
            <span class="workspace-tree-name">${escapeHtml(entry.name)}</span>
            ${meta}
          </button>
        `
        : `
          <button class="workspace-tree-row file-row ${escapeHtml(entry.kind || 'other')}${isActiveFile ? ' selected' : ''}" type="button" style="${indentStyle}" data-depth="${depth}" data-workspace-file="${escapeHtml(entryPath)}">
            <span class="workspace-tree-guide"></span>
            <span class="workspace-chevron"></span>
            <span class="workspace-file-icon">${workspaceKindIcon(entry)}</span>
            <span class="workspace-tree-name">${escapeHtml(entry.name)}</span>
            ${meta}
          </button>
        `;
      const children = isExpanded
        ? `<div class="workspace-tree-children">${renderWorkspaceTree(key, entryPath, depth + 1)}</div>`
        : '';
      return `${row}${children}`;
    })
    .join('');
}

function renderWorkspaceCard(status) {
  const key = selectedKey();
  const listing = workspaceListing(key, '');
  const loading = workspaceIsLoading(key, '');
  const error = workspaceError(key, '');
  const root = listing?.workspace_root || status?.workspace || 'workspace pending';
  const remote = listing?.remote;
  const count = Number(listing?.total_entries || listing?.returned_entries || 0);
  const rootLabel = remote?.cwd || listing?.path || compactPath(root);
  const body = error
    ? `<div class="workspace-empty error">${escapeHtml(error)}</div>`
    : loading && !listing
      ? '<div class="workspace-empty">正在加载 workspace...</div>'
      : `<div class="workspace-tree" role="tree">${renderWorkspaceTree(key)}</div>`;
  const footer = listing?.truncated
    ? `<div class="workspace-footer">已显示 ${Number(listing.returned_entries || 0)} / ${Number(listing.total_entries || 0)} 项</div>`
    : listing
      ? `<div class="workspace-footer">${Number(listing.returned_entries || 0)} 项</div>`
      : '';
  return `
    <section class="workspace-page">
      <div class="workspace-tree-head">
        <div>
          <div class="workspace-tree-title">工作区文件 <span>${count || ''}</span></div>
          <div class="workspace-mode">${remote ? `${escapeHtml(remote.host)} · ${escapeHtml(remote.cwd)}` : 'local workspace'}</div>
        </div>
        <button class="icon-button workspace-refresh" type="button" title="刷新文件树" data-workspace-refresh>${icons.refresh}</button>
      </div>
      <label class="workspace-search">
        <span>${icons.search}</span>
        <input id="workspaceFilterInput" type="search" value="${escapeHtml(state.workspaceFilter)}" placeholder="筛选文件..." />
      </label>
      <div class="workspace-root-group">
        <div class="workspace-root-row">
          <span class="workspace-root-chevron">${icons.chevronDown}</span>
          <span>${escapeHtml(rootLabel || 'workspace')}</span>
        </div>
        <div class="workspace-root-line">${escapeHtml(root)}</div>
      </div>
      ${body}
      ${footer}
    </section>
  `;
}

function toggleWorkspaceDirectory(path) {
  const key = selectedKey();
  if (!key) {
    return Promise.resolve();
  }
  const normalized = normalizeWorkspacePath(path);
  const expanded = workspaceExpandedSet(key);
  state.workspacePaths.set(key, normalized);
  if (expanded.has(normalized)) {
    if (normalized !== '') {
      expanded.delete(normalized);
    }
    renderContext();
    return Promise.resolve();
  }
  expanded.add(normalized);
  if (workspaceListing(key, normalized)) {
    renderContext();
    return Promise.resolve();
  }
  return refreshWorkspace(normalized, { expand: true, setActive: true });
}

function bindWorkspaceActions() {
  const root = elements.fileContent || elements.contextContent;
  root.querySelector('#workspaceFilterInput')?.addEventListener('input', (event) => {
    const cursor = event.target.selectionStart;
    state.workspaceFilter = event.target.value;
    renderFilesContext();
    const nextInput = root.querySelector('#workspaceFilterInput');
    nextInput?.focus();
    nextInput?.setSelectionRange(cursor, cursor);
  });
  root.querySelectorAll('[data-workspace-toggle]').forEach((button) => {
    button.addEventListener('click', () => {
      toggleWorkspaceDirectory(button.dataset.workspaceToggle || '').catch((error) => showToast(error.message));
    });
  });
  root.querySelectorAll('[data-workspace-file]').forEach((button) => {
    button.addEventListener('click', () => {
      selectWorkspaceFile(button.dataset.workspaceFile || '').catch((error) => showToast(error.message));
    });
  });
  root.querySelector('[data-workspace-refresh]')?.addEventListener('click', () => {
    refreshWorkspace(currentWorkspacePath(), { expand: true, setActive: true }).catch((error) => showToast(error.message));
  });

  // Right-click context menu on workspace entries
  root.querySelectorAll('[data-workspace-toggle], [data-workspace-file]').forEach((button) => {
    button.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      event.stopPropagation();
      const path = button.dataset.workspaceToggle || button.dataset.workspaceFile || '';
      const isDir = button.dataset.workspaceToggle !== undefined;
      showWorkspaceContextMenu(event.clientX, event.clientY, path, isDir);
    });
  });

  // Drag-and-drop upload on workspace page
  const workspacePage = root.querySelector('.workspace-page');
  if (workspacePage) {
    bindWorkspaceDragDrop(workspacePage);
  }
}

function showWorkspaceContextMenu(x, y, path, isDir) {
  dismissWorkspaceContextMenu();
  const menu = document.createElement('div');
  menu.className = 'workspace-context-menu';
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  menu.innerHTML = `
    <button type="button" data-action="download">${isDir ? '下载目录 (tar.gz)' : '下载文件 (tar.gz)'}</button>
  `;
  document.body.append(menu);
  // Adjust if menu overflows viewport
  requestAnimationFrame(() => {
    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) menu.style.left = `${window.innerWidth - rect.width - 8}px`;
    if (rect.bottom > window.innerHeight) menu.style.top = `${window.innerHeight - rect.height - 8}px`;
  });
  menu.querySelector('[data-action="download"]')?.addEventListener('click', () => {
    dismissWorkspaceContextMenu();
    downloadWorkspaceEntry(path).catch((error) => showToast(error.message));
  });
  const dismiss = (event) => {
    if (!menu.contains(event.target)) {
      dismissWorkspaceContextMenu();
    }
  };
  setTimeout(() => document.addEventListener('click', dismiss, { once: true }), 0);
  state._workspaceContextMenu = menu;
}

function dismissWorkspaceContextMenu() {
  if (state._workspaceContextMenu) {
    state._workspaceContextMenu.remove();
    state._workspaceContextMenu = null;
  }
}

function bindWorkspaceDragDrop(container) {
  let dragCounter = 0;
  container.addEventListener('dragenter', (event) => {
    event.preventDefault();
    dragCounter++;
    container.classList.add('drag-over');
  });
  container.addEventListener('dragleave', (event) => {
    event.preventDefault();
    dragCounter--;
    if (dragCounter <= 0) {
      dragCounter = 0;
      container.classList.remove('drag-over');
    }
  });
  container.addEventListener('dragover', (event) => {
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
  });
  container.addEventListener('drop', (event) => {
    event.preventDefault();
    dragCounter = 0;
    container.classList.remove('drag-over');
    const items = event.dataTransfer?.items;
    if (!items || items.length === 0) return;
    const targetDir = currentWorkspacePath();
    collectDroppedFiles(items).then((fileEntries) => {
      if (fileEntries.length > 0) {
        uploadFilesToWorkspace(fileEntries, targetDir).catch((error) => showToast(error.message));
      }
    });
  });
}

async function collectDroppedFiles(dataTransferItems) {
  const entries = [];
  const items = [];
  for (let i = 0; i < dataTransferItems.length; i++) {
    const item = dataTransferItems[i];
    if (item.kind === 'file') {
      const entry = item.webkitGetAsEntry ? item.webkitGetAsEntry() : null;
      if (entry) {
        items.push(entry);
      } else {
        const file = item.getAsFile();
        if (file) {
          const data = await file.arrayBuffer();
          entries.push({ relativePath: file.name, data, isDirectory: false });
        }
      }
    }
  }
  for (const entry of items) {
    await traverseEntry(entry, '', entries);
  }
  return entries;
}

async function traverseEntry(entry, parentPath, results) {
  const fullPath = parentPath ? `${parentPath}/${entry.name}` : entry.name;
  if (entry.isFile) {
    const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
    const data = await file.arrayBuffer();
    results.push({ relativePath: fullPath, data, isDirectory: false });
  } else if (entry.isDirectory) {
    results.push({ relativePath: fullPath + '/', data: new ArrayBuffer(0), isDirectory: true });
    const reader = entry.createReader();
    const children = await new Promise((resolve, reject) => {
      const all = [];
      function readBatch() {
        reader.readEntries((batch) => {
          if (batch.length === 0) {
            resolve(all);
          } else {
            all.push(...batch);
            readBatch();
          }
        }, reject);
      }
      readBatch();
    });
    for (const child of children) {
      await traverseEntry(child, fullPath, results);
    }
  }
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
  const rendered = renderStructuredMessageContent(message, detail);
  const hasStructuredFileItems = (detail?.items || message.items || []).some((item) => item.type === 'file');
  const attachmentsHtml = hasStructuredFileItems ? '' : renderMessageAttachments(message, detail);
  const loadingHtml = messageId && state.messageDetailsLoading.has(messageId)
    ? '<div class="message-attachments muted">正在载入详情...</div>'
    : '';
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
      ${loadingHtml}
      ${rendered}
      ${attachmentsHtml}
    </section>
  `;
  elements.contextContent.querySelectorAll('[data-workspace-file]').forEach((button) => {
    button.addEventListener('click', () => {
      selectWorkspaceFile(button.dataset.workspaceFile || '').catch((error) => showToast(error.message));
    });
  });
}

function languageClass(path) {
  const ext = fileExtension(path);
  if (['js', 'jsx', 'ts', 'tsx', 'json', 'css', 'html', 'rs', 'py', 'sh', 'bash', 'zsh', 'md'].includes(ext)) {
    return `language-${ext}`;
  }
  return 'language-text';
}

function renderEditorTabs() {
  const tabs = selectedFileTabs();
  if (tabs.length === 0) {
    return '';
  }
  return `
    <div class="editor-tabs">
      ${tabs
        .map((tab) => {
          const active = normalizeWorkspacePath(tab.path) === normalizeWorkspacePath(state.activePreviewFilePath);
          return `
            <button class="editor-tab${active ? ' active' : ''}" type="button" data-editor-tab="${escapeHtml(tab.path)}">
              <span>${escapeHtml(fileNameFromPath(tab.path))}</span>
              <span class="editor-tab-close" data-editor-close="${escapeHtml(tab.path)}">×</span>
            </button>
          `;
        })
        .join('')}
    </div>
  `;
}

function highlightedCode(value, path) {
  const escaped = escapeHtml(value || '');
  const ext = fileExtension(path);
  const keywordPattern =
    ext === 'rs'
      ? /\b(fn|let|mut|pub|struct|enum|impl|trait|use|mod|match|if|else|for|while|loop|return|async|await|Result|Option|Some|None|Ok|Err)\b/g
      : ext === 'py'
        ? /\b(def|class|import|from|as|if|elif|else|for|while|return|try|except|with|async|await|True|False|None)\b/g
        : ext === 'css'
          ? /\b(display|position|grid|flex|color|background|border|padding|margin|width|height|min|max|overflow|font|transform|transition)\b/g
          : /\b(const|let|var|function|return|if|else|for|while|class|import|export|from|async|await|new|try|catch|throw|true|false|null|undefined)\b/g;
  return escaped
    .replace(/(&quot;.*?&quot;|&#039;.*?&#039;|`.*?`)/g, '<span class="code-string">$1</span>')
    .replace(/(\/\/.*?$|#.*?$)/gm, '<span class="code-comment">$1</span>')
    .replace(keywordPattern, '<span class="code-keyword">$1</span>')
    .replace(/\b(\d+(?:\.\d+)?)\b/g, '<span class="code-number">$1</span>');
}

function renderFileBody(path, preview, loading, error) {
  if (error) {
    return `<div class="workspace-empty error">${escapeHtml(error)}</div>`;
  }
  if (loading && !preview) {
    return '<div class="workspace-empty">正在读取文件...</div>';
  }
  if (!preview) {
    return '<div class="workspace-empty">从左侧文件树选择一个文件。</div>';
  }
  if (preview.encoding === 'base64') {
    if (isImageFile(path)) {
      return `
        <div class="file-image-preview">
          <img src="data:${escapeHtml(imageMimeType(path))};base64,${escapeHtml(preview.data || '')}" alt="${escapeHtml(fileNameFromPath(path))}" />
        </div>
      `;
    }
    return `<div class="workspace-empty">二进制文件，已读取 ${escapeHtml(formatBytes(preview.returned_bytes))}。</div>`;
  }
  const source = preview.data || '';
  const mode = currentFileViewMode(path);
  if (isMarkdownFile(path) && mode === 'preview') {
    return `<article class="markdown-preview">${markdownToHtml(source)}</article>`;
  }
  return `
    <pre class="file-code-view ${escapeHtml(languageClass(path))}"><code>${highlightedCode(source, path)}</code></pre>
  `;
}

function renderFilePreviewShell() {
  const key = selectedKey();
  const path = normalizeWorkspacePath(state.activePreviewFilePath);
  if (!path) {
    return `
      <section class="editor-shell">
        ${renderEditorTabs()}
        <div class="context-empty">
          <strong>未打开文件</strong>
          <span>从文件树选择文件后会在这里预览。</span>
        </div>
      </section>
    `;
  }
  const preview = workspaceFilePreview(key, path);
  const error = workspaceFileError(key, path);
  const loading = workspaceFileIsLoading(key, path);
  const name = preview?.name || path.split('/').filter(Boolean).at(-1) || path;
  const meta = preview
    ? `${formatBytes(preview.size_bytes)}${preview.truncated ? ' · truncated preview' : ''}`
    : loading
      ? 'loading preview'
      : 'file preview';
  const mode = currentFileViewMode(path);
  const canPreview = isMarkdownFile(path);
  return `
    <section class="editor-shell">
      ${renderEditorTabs()}
      <div class="editor-toolbar">
        <div class="editor-title">
          <strong>${escapeHtml(name)}</strong>
          <span>${escapeHtml(path)} · ${escapeHtml(meta)}</span>
        </div>
        <div class="editor-actions">
          ${
            canPreview
              ? `<button class="tiny-button${mode === 'source' ? ' active' : ''}" type="button" data-file-mode="source">源码</button>
                 <button class="tiny-button${mode === 'preview' ? ' active' : ''}" type="button" data-file-mode="preview">预览</button>`
              : ''
          }
        </div>
      </div>
      <div class="editor-body">
        ${renderFileBody(path, preview, loading, error)}
      </div>
    </section>
  `;
}

function renderFileDetailContext() {
  elements.contextContent.innerHTML = renderFilePreviewShell();
  bindEditorActions(elements.contextContent);
}

function bindEditorActions(root = elements.contextContent) {
  root.querySelectorAll('[data-editor-tab]').forEach((button) => {
    button.addEventListener('click', () => {
      state.activePreviewFilePath = normalizeWorkspacePath(button.dataset.editorTab || '');
      renderContext();
    });
  });
  root.querySelectorAll('[data-editor-close]').forEach((button) => {
    button.addEventListener('click', (event) => {
      event.stopPropagation();
      closeFileTab(button.dataset.editorClose || '');
    });
  });
  root.querySelectorAll('[data-file-mode]').forEach((button) => {
    button.addEventListener('click', () => {
      setFileViewMode(state.activePreviewFilePath, button.dataset.fileMode || 'source');
      renderContext();
    });
  });
}

function terminalKey() {
  return selectedKey();
}

function activeTerminal() {
  const list = state.terminals.get(terminalKey()) || [];
  return list.find((terminal) => terminal.terminal_id === state.activeTerminalId) || list[0] || null;
}

function terminalOutputKey(terminal) {
  const key = terminalKey();
  return terminal ? `${key}:${terminal.terminal_id}` : '';
}

function updateTerminalSummary(summary) {
  if (!summary || !state.selected) {
    return;
  }
  const key = terminalKey();
  const list = state.terminals.get(key) || [];
  const index = list.findIndex((terminal) => terminal.terminal_id === summary.terminal_id);
  if (index >= 0) {
    list[index] = { ...list[index], ...summary };
  } else {
    list.push(summary);
  }
  state.terminals.set(key, list);
}

function estimateTerminalSize() {
  const target = elements.terminalContent;
  const rect = target?.getBoundingClientRect?.();
  const width = Math.max(360, Number(rect?.width || window.innerWidth || 960));
  const height = Math.max(160, Number(rect?.height || state.layout.terminal || 240) - 34);
  return {
    cols: Math.max(40, Math.min(220, Math.floor((width - 24) / 7.1))),
    rows: Math.max(8, Math.min(80, Math.floor((height - 16) / 14.4)))
  };
}

function terminalCreatePayload(preferredShell = null) {
  const payload = estimateTerminalSize();
  if (preferredShell) {
    payload.shell = preferredShell;
  }
  return payload;
}

function disposeXtermSessions() {
  for (const session of state.xtermSessions.values()) {
    session.dataDisposable?.dispose?.();
    session.resizeDisposable?.dispose?.();
    session.fitTimer && clearTimeout(session.fitTimer);
    session.resizeTimer && clearTimeout(session.resizeTimer);
    session.terminal?.dispose?.();
  }
  state.xtermSessions.clear();
  for (const queue of state.terminalInputQueues.values()) {
    queue.timer && clearTimeout(queue.timer);
  }
  state.terminalInputQueues.clear();
}

function activeXtermSession() {
  const terminal = activeTerminal();
  return terminal ? state.xtermSessions.get(terminalOutputKey(terminal)) : null;
}

function fitActiveXterm() {
  const session = activeXtermSession();
  if (!session) {
    return;
  }
  requestAnimationFrame(() => {
    try {
      session.fitAddon?.fit?.();
    } catch {
      // xterm may not have measured fonts yet; the next render/poll will fit again.
    }
  });
}

async function resizeTerminal(terminalId, cols, rows) {
  if (!state.selected || !terminalId || !cols || !rows) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  const response = await api(serverId, `/api/conversations/${conversationId}/terminals/${terminalId}/resize`, {
    method: 'POST',
    body: { cols, rows }
  });
  updateTerminalSummary(response.data);
}

function syncXtermOutput(session, output) {
  // Called with the full accumulated output for fallback rendering.
  // For xterm sessions, prefer writeXtermIncremental() during poll.
  const text = safeText(output);
  if (text.length < session.writtenLength) {
    session.terminal.reset();
    session.writtenLength = 0;
  }
  const next = text.slice(session.writtenLength);
  if (next) {
    session.terminal.write(next);
    session.writtenLength = text.length;
  }
}

function writeXtermIncremental(session, incrementalData) {
  if (!incrementalData) return;
  session.terminal.write(incrementalData);
  session.writtenLength += incrementalData.length;
}

function createXtermSession(host, terminal, output) {
  const TerminalCtor = window.Terminal?.Terminal || window.Terminal;
  const FitAddonCtor = window.FitAddon?.FitAddon || window.FitAddon;
  const WebglAddonCtor = window.WebglAddon?.WebglAddon || window.WebglAddon;
  if (!TerminalCtor || !FitAddonCtor) {
    return null;
  }
  const xterm = new TerminalCtor({
    allowTransparency: true,
    cursorBlink: true,
    cursorStyle: 'bar',
    fontFamily: '"SFMono-Regular", "Cascadia Code", "Roboto Mono", ui-monospace, monospace',
    fontSize: 13,
    lineHeight: 1.15,
    scrollback: 10000,
    theme: {
      background: 'rgba(8, 9, 8, 0.72)',
      foreground: '#e9e9e3',
      cursor: '#e9e9e3',
      selectionBackground: '#3f4644',
      black: '#232623',
      red: '#ff6b6b',
      green: '#5ad690',
      yellow: '#e6c964',
      blue: '#7aa2ff',
      magenta: '#d58cff',
      cyan: '#66d9ef',
      white: '#e9e9e3',
      brightBlack: '#7a8078',
      brightRed: '#ff8585',
      brightGreen: '#78e6a7',
      brightYellow: '#f1d981',
      brightBlue: '#92b6ff',
      brightMagenta: '#e5a3ff',
      brightCyan: '#8defff',
      brightWhite: '#ffffff'
    }
  });
  const fitAddon = new FitAddonCtor();
  xterm.loadAddon(fitAddon);
  xterm.open(host);
  // Try loading WebGL renderer for better performance.
  if (WebglAddonCtor) {
    try {
      const webglAddon = new WebglAddonCtor();
      xterm.loadAddon(webglAddon);
    } catch {
      // WebGL not available; fall back to canvas renderer.
    }
  }
  const session = {
    terminal: xterm,
    fitAddon,
    writtenLength: 0,
    resizeTimer: null,
    dataDisposable: null,
    resizeDisposable: null
  };
  session.dataDisposable = xterm.onData((data) => {
    queueTerminalInput(data);
  });
  session.resizeDisposable = xterm.onResize(({ cols, rows }) => {
    clearTimeout(session.resizeTimer);
    session.resizeTimer = setTimeout(() => {
      resizeTerminal(terminal.terminal_id, cols, rows).catch(() => {});
    }, 160);
  });
  requestAnimationFrame(() => {
    try {
      fitAddon.fit();
      resizeTerminal(terminal.terminal_id, xterm.cols, xterm.rows).catch(() => {});
    } catch {
      // Best effort; xterm will still render with its initial geometry.
    }
    syncXtermOutput(session, output);
    xterm.focus();
  });
  return session;
}

async function refreshTerminals() {
  if (!state.selected) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  const key = terminalKey();
  const response = await api(serverId, `/api/conversations/${conversationId}/terminals`);
  const terminals = response.data?.terminals || [];
  state.terminals.set(key, terminals);
  if (!state.activeTerminalId || !terminals.some((terminal) => terminal.terminal_id === state.activeTerminalId)) {
    state.activeTerminalId = terminals[0]?.terminal_id || null;
  }
}

async function createTerminal() {
  if (!state.selected) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  const status = selectedStatus();
  const preferZsh = isRemoteStatus(status);
  let response;
  try {
    response = await api(serverId, `/api/conversations/${conversationId}/terminals`, {
      method: 'POST',
      body: terminalCreatePayload(preferZsh ? 'zsh' : null)
    });
  } catch (error) {
    if (!preferZsh) {
      throw error;
    }
    response = await api(serverId, `/api/conversations/${conversationId}/terminals`, {
      method: 'POST',
      body: terminalCreatePayload()
    });
  }
  updateTerminalSummary(response.data);
  state.activeTerminalId = response.data?.terminal_id || state.activeTerminalId;
  renderTerminalContext();
  startTerminalPoll();
}

async function readTerminalOutput() {
  if (!state.selected) {
    return null;
  }
  const terminal = activeTerminal();
  if (!terminal) {
    return null;
  }
  const { serverId, conversationId } = state.selected;
  const key = terminalKey();
  const outputKey = `${key}:${terminal.terminal_id}`;
  const offset = state.terminalOffsets.get(outputKey) ?? 0;
  const response = await api(
    serverId,
    `/api/conversations/${conversationId}/terminals/${terminal.terminal_id}/output?offset=${offset}&limit_bytes=262144`
  );
  const output = response.data;
  const previous = state.terminalOutput.get(outputKey) || '';
  let incrementalData = '';
  if (output?.data) {
    incrementalData = output.data;
    const nextText = output.dropped_bytes > 0 ? output.data : `${previous}${output.data}`;
    state.terminalOutput.set(outputKey, nextText.slice(-524288));
  }
  if (typeof output?.running === 'boolean') {
    updateTerminalSummary({ ...terminal, running: output.running, next_offset: output.next_offset });
  }
  state.terminalOffsets.set(outputKey, output?.next_offset ?? offset);
  return { incrementalData, dropped: (output?.dropped_bytes || 0) > 0 };
}

function startTerminalPoll() {
  clearTerminalPoll();
  if (!state.terminalOpen || !state.selected) {
    return;
  }
  const key = selectedKey();
  let slowTick = 0;
  const tick = async () => {
    if (!state.terminalOpen || selectedKey() !== key) {
      clearTerminalPoll();
      return;
    }
    let hasNewData = false;
    try {
      slowTick += 1;
      if (slowTick === 1 || slowTick % 10 === 0) {
        await refreshTerminals();
      }
      const result = await readTerminalOutput();
      const incremental = result?.incrementalData || '';
      hasNewData = incremental.length > 0;
      // Write incremental data directly to xterm if session exists.
      const session = activeXtermSession();
      if (session && hasNewData) {
        if (result.dropped) {
          // Buffer was truncated server-side; reset and write fresh.
          session.terminal.reset();
          session.writtenLength = 0;
          const terminal = activeTerminal();
          const outputKey = terminal ? terminalOutputKey(terminal) : '';
          const fullOutput = outputKey ? state.terminalOutput.get(outputKey) || '' : '';
          session.terminal.write(fullOutput);
          session.writtenLength = fullOutput.length;
        } else {
          writeXtermIncremental(session, incremental);
        }
      }
      renderTerminalContext();
    } catch (error) {
      showToast(error.message);
    }
    // Adaptive: poll faster when receiving data, slower when idle.
    const interval = !document.hasFocus() ? 700 : hasNewData ? 32 : 120;
    state.terminalPoll = setTimeout(tick, interval);
  };
  state.terminalPoll = setTimeout(tick, 32);
}

async function sendTerminalInput(value, options = {}) {
  if (!state.selected || !value) {
    return;
  }
  const terminal = activeTerminal();
  if (!terminal) {
    return;
  }
  const { serverId, conversationId } = state.selected;
  const response = await api(serverId, `/api/conversations/${conversationId}/terminals/${terminal.terminal_id}/input`, {
    method: 'POST',
    body: { data: options.raw ? value : `${value}\n` }
  });
  updateTerminalSummary(response.data);
  if (!options.raw) {
    await readTerminalOutput();
    renderTerminalContext();
  }
}

function queueTerminalInput(value) {
  if (!state.selected || !value) {
    return;
  }
  const terminal = activeTerminal();
  if (!terminal) {
    return;
  }
  const outputKey = terminalOutputKey(terminal);
  const queue = state.terminalInputQueues.get(outputKey) || { value: '', timer: null };
  queue.value += value;
  clearTimeout(queue.timer);
  queue.timer = setTimeout(() => {
    const payload = queue.value;
    queue.value = '';
    sendTerminalInput(payload, { raw: true }).catch((error) => showToast(error.message));
  }, 24);
  state.terminalInputQueues.set(outputKey, queue);
}

function bindTerminalActions(target) {
  target.querySelector('[data-terminal-new]')?.addEventListener('click', () => {
    createTerminal().catch((error) => showToast(error.message));
  });
  target.querySelector('[data-terminal-form]')?.addEventListener('submit', (event) => {
    event.preventDefault();
    const input = target.querySelector('#terminalInput');
    const value = input?.value || '';
    if (input) {
      input.value = '';
    }
    sendTerminalInput(value).catch((error) => showToast(error.message));
  });
  target.querySelector('#terminalInput')?.addEventListener('keydown', (event) => {
    if (event.ctrlKey && event.key.toLowerCase() === 'c') {
      event.preventDefault();
      sendTerminalInput('\u0003', { raw: true }).catch((error) => showToast(error.message));
    } else if (event.ctrlKey && event.key.toLowerCase() === 'd') {
      event.preventDefault();
      sendTerminalInput('\u0004', { raw: true }).catch((error) => showToast(error.message));
    } else if (event.key === 'Tab') {
      event.preventDefault();
      sendTerminalInput('\t', { raw: true }).catch((error) => showToast(error.message));
    }
  });
  target.querySelector('[data-terminal-frame]')?.addEventListener('click', () => {
    activeXtermSession()?.terminal?.focus?.();
    target.querySelector('#terminalInput')?.focus();
  });
}

function scrollTerminalOutput(target) {
  const frame = target.querySelector('[data-terminal-frame]');
  if (frame) {
    frame.scrollTop = frame.scrollHeight;
  }
}

function renderTerminalContext() {
  const target = elements.terminalContent;
  if (!target) {
    return;
  }
  if (!state.selected) {
    target.innerHTML = `
      <div class="context-empty">
        <strong>没有终端</strong>
        <span>选择一个 Conversation 后启动终端。</span>
      </div>
    `;
    return;
  }
  const terminal = activeTerminal();
  const key = terminalKey();
  const outputKey = terminal ? `${key}:${terminal.terminal_id}` : '';
  const output = terminal ? state.terminalOutput.get(outputKey) || '' : '';
  const existingFrame = target.querySelector('[data-terminal-frame]');
  if (terminal && existingFrame?.dataset.terminalId === terminal.terminal_id) {
    const subtitle = target.querySelector('.terminal-subtitle');
    if (subtitle) {
      subtitle.textContent = `${terminal.cwd} · ${terminal.running ? 'running' : 'exited'}`;
    }
    const session = state.xtermSessions.get(outputKey);
    if (!session) {
      setupTerminalFrame(target, terminal, output);
    }
    fitActiveXterm();
    return;
  }
  target.innerHTML = `
    <section class="terminal-page">
      <div class="terminal-head">
        <div>
          <div class="terminal-title">终端</div>
          <div class="terminal-subtitle">${terminal ? `${escapeHtml(terminal.cwd)} · ${terminal.running ? 'running' : 'exited'}` : '未启动'}</div>
        </div>
        <button class="primary-button terminal-new" type="button" data-terminal-new>${terminal ? '新建' : '启动'}</button>
      </div>
      ${
        terminal
          ? `
            <div class="terminal-frame" data-terminal-frame data-terminal-id="${escapeHtml(terminal.terminal_id)}">
              <div class="terminal-xterm" data-terminal-xterm></div>
            </div>
          `
          : '<div class="workspace-empty">启动一个终端来操作当前 workspace。</div>'
      }
    </section>
  `;
  bindTerminalActions(target);
  if (terminal) {
    setupTerminalFrame(target, terminal, output);
  }
}

function setupTerminalFrame(target, terminal, output) {
  const host = target.querySelector('[data-terminal-xterm]');
  if (!host) {
    return;
  }
  const outputKey = terminalOutputKey(terminal);
  if (state.xtermSessions.has(outputKey)) {
    return;
  }
  const session = createXtermSession(host, terminal, output);
  if (session) {
    state.xtermSessions.set(outputKey, session);
    return;
  }
  host.innerHTML = `<pre class="terminal-output"><code>${escapeHtml(terminalDisplayOutput(normalizeTerminalOutput(output || '')))}</code></pre>`;
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

function bindLayoutResizers() {
  document.querySelectorAll('[data-resizer]').forEach((handle) => {
    handle.addEventListener('pointerdown', (event) => {
      event.preventDefault();
      handle.setPointerCapture(event.pointerId);
      document.body.classList.add('is-resizing');
      const type = handle.dataset.resizer;
      const start = {
        x: event.clientX,
        y: event.clientY,
        sidebar: state.layout.sidebar,
        context: state.layout.context,
        file: state.layout.file,
        terminal: state.layout.terminal,
        workbench: document.querySelector('.workbench')?.getBoundingClientRect()
      };
      const move = (moveEvent) => {
        const workbench = start.workbench;
        if (type === 'sidebar') {
          if (state.settings?.sidebarCollapsed) {
            return;
          }
          state.layout.sidebar = clampNumber(start.sidebar + moveEvent.clientX - start.x, 180, 520);
        } else if (type === 'context' && workbench) {
          const rightEdge = workbench.right - (state.fileBarOpen ? state.layout.file : 0);
          state.layout.context = clampNumber(rightEdge - moveEvent.clientX, 220, 800);
        } else if (type === 'file' && workbench) {
          state.layout.file = clampNumber(workbench.right - moveEvent.clientX, 220, 800);
        } else if (type === 'terminal' && workbench) {
          state.layout.terminal = clampNumber(workbench.bottom - moveEvent.clientY, 120, 800);
        }
        applyLayoutSettings();
      };
      const finish = () => {
        document.body.classList.remove('is-resizing');
        saveLayoutSettings();
        window.removeEventListener('pointermove', move);
        window.removeEventListener('pointerup', finish);
        window.removeEventListener('pointercancel', finish);
      };
      window.addEventListener('pointermove', move);
      window.addEventListener('pointerup', finish);
      window.addEventListener('pointercancel', finish);
    });
  });
}

function autosizeComposer() {
  elements.composerInput.style.height = 'auto';
  elements.composerInput.style.height = `${Math.min(Math.max(elements.composerInput.scrollHeight, 58), 150)}px`;
}

async function init() {
  loadLayoutSettings();
  applyLayoutSettings();
  state.settings = await window.stellacode.loadSettings();
  ensureLocalSettings();
  state.activeServerId = state.settings.activeServerId;
  applyLayoutSettings();
  bindEvents();
  renderSidebar();
  renderHeader();
  renderMessages();
  renderContext();
  await refreshAllServers();
}

function bindEvents() {
  bindLayoutResizers();
  elements.toggleSidebarButton.addEventListener('click', () => {
    closeConversationMenu();
    state.settings.sidebarCollapsed = !state.settings.sidebarCollapsed;
    applyLayoutSettings();
    saveSettingsSoon();
  });
  elements.newConversationButton.addEventListener('click', openNewConversationModal);
  elements.settingsButton.addEventListener('click', openSettingsModal);
  elements.refreshButton.addEventListener('click', async () => {
    await refreshConversation();
    await refreshWorkspace();
  });
  elements.toggleContextButton.addEventListener('click', () => {
    state.contextCollapsed = !state.contextCollapsed;
    if (!state.contextCollapsed) {
      state.fileBarOpen = false;
    }
    renderContext();
  });
  elements.toggleFileButton.addEventListener('click', () => {
    state.fileBarOpen = !state.fileBarOpen;
    if (state.fileBarOpen) {
      state.contextCollapsed = true;
    }
    renderContext();
    if (state.fileBarOpen) {
      refreshWorkspace(currentWorkspacePath(), { expand: true, setActive: true }).catch((error) => showToast(error.message));
    }
  });
  elements.toggleTerminalButton.addEventListener('click', () => {
    state.terminalOpen = !state.terminalOpen;
    renderContext();
    if (state.terminalOpen) {
      refreshTerminals()
        .then(async () => {
          if (!activeTerminal()) {
            await createTerminal();
          }
          renderTerminalContext();
          startTerminalPoll();
        })
        .catch((error) => showToast(error.message));
    } else {
      clearTerminalPoll();
    }
  });
  elements.sendButton.addEventListener('click', sendMessage);
  elements.attachButton.addEventListener('click', () => showToast('文件上下文会跟随后端 API 一起接入。'));
  elements.messageList.addEventListener('scroll', () => {
    if (elements.messageList.scrollTop < 80) {
      loadOlderMessages();
    }
  });
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
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      sendMessage();
    }
  });
  window.addEventListener('focus', () => {
    if (!state.selected) {
      return;
    }
    refreshConversation().catch((error) => showToast(error.message));
    refreshWorkspace(currentWorkspacePath(), { setActive: false }).catch(() => {});
  });
  window.addEventListener('resize', () => {
    closeConversationMenu();
    applyLayoutSettings();
  });
  document.addEventListener('click', closeConversationMenu);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      closeConversationMenu();
    }
  });
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible' && state.selected) {
      refreshConversation().catch((error) => showToast(error.message));
    }
  });
  elements.modalLayer.addEventListener('click', (event) => {
    if (event.target === elements.modalLayer) {
      closeModal();
    }
  });
}

init().catch((error) => showToast(error.message));
