import React, { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import 'highlight.js/styles/github-dark.css';
import './styles.css';
import {
  conversationKey,
  conversationStreamUrl,
  createConversation,
  createForegroundSession,
  deleteConversation,
  deleteForegroundSession,
  displayConversationName,
  displayForegroundSessionName,
  foregroundSessions,
  markConversationSeen,
  loadConversations,
  loadMessages,
  loadModels,
  loadWorkspace,
  loadWorkspaceFile,
  normalizeConversationSummary,
  normalizeForegroundSessionSummary,
  postConversationMessage,
  renameConversation,
  renameForegroundSession,
  selectedForegroundSessionId
} from './lib/api';
import { ConversationBar } from './components/ConversationBar';
import { WindowChrome } from './components/WindowChrome';
import { ChatWorkspace } from './components/ChatWorkspace';
import { OverviewPanel } from './components/OverviewPanel';
import { WorkspacePanel } from './components/WorkspacePanel';
import { FilePreviewPanel } from './components/FilePreviewPanel';
import { TerminalDock } from './components/TerminalDock';
import { NewConversationDialog } from './components/NewConversationDialog';
import { RenameConversationDialog, RenameSessionDialog } from './components/RenameConversationDialog';
import { ConversationPropertiesDialog } from './components/ConversationPropertiesDialog';
import { SettingsDialog } from './components/SettingsDialog';
import { clamp, formatBytes, formatModel, statusUsageTotals } from './lib/format';
import { attachmentName, fileExtension, fileNameFromPath, imageMimeType } from './lib/fileUtils';
import { activityFromMessages, addUsageTotals, committedMessageProtocolMismatches, firstMessageId, hasOlderMessages, isFinalAssistantMessage, lastServerMessageIndex, liveActivitySignature, mergeMessages, messageIndex, messageOrderFromId, shortText, usageDeltaFromMessages } from './lib/messageUtils';
import {
  appendStreamAssistantDelta,
  appendStreamReasoningSummary,
  appendStreamToolCallDelta,
  appendStreamToolResultDone,
  applyStreamErrorToMessages,
  createStreamBufferStore,
  createStreamIndexTracker,
  markQueuedUserMessage,
  normalizedStreamEvent,
  removeStreamingMessagesForTurn,
  streamActivityBaseId,
  streamDeltaText,
  streamErrorText,
  streamEventType,
  streamFinalizedActivityIds,
  streamItemId
} from './lib/chatStreamDataPlane';
import { startChatSocketClient } from './lib/chatSocketClient';
import { readMessageCache, removeMessageCache, writeMessageCache } from './lib/chatMessageCache';
import { chatSessionStateIsActive, chatSnapshotState, isActiveSessionState, mergeProgressActivity, normalizeProgressFeedback, recentMessagePageParams } from './lib/chatSessionState';
import { localCacheKey, readLocalCache, removeLocalCache, writeLocalCache } from './lib/localCache';
import { effectiveThemeMode, themeCssVariables } from './lib/theme';
import { collectDroppedFiles, packFilesToTarGz, uploadPayloadStats } from './lib/uploadArchive';
import { normalizeWorkspacePath, parentWorkspacePath, workspaceDisplayRoot, workspaceEntryKind, workspaceFileKind } from './lib/workspaceUtils';

const SIDEBAR_EXPANDED = 286;
const SIDEBAR_COLLAPSED = 0;
const WORKSPACE_PANEL_MIN = 340;
const WORKSPACE_PANEL_MAX = 620;
const TERMINAL_HEIGHT_MIN = 160;
const TERMINAL_HEIGHT_MAX = 620;
const TERMINAL_LIST_MIN = 180;
const TERMINAL_LIST_MAX = 360;
const MAX_UPLOAD_COMPRESSED_BYTES = 10 * 1024 * 1024;
const PDF_PREVIEW_MAX_BYTES = 50 * 1024 * 1024;
const MESSAGE_IMAGE_PREVIEW_MAX_BYTES = 20 * 1024 * 1024;
const MIN_DISPLAY_FONT_SIZE = 11;
const MAX_DISPLAY_FONT_SIZE = 18;
const MIN_UI_SCALE = 0.8;
const MAX_UI_SCALE = 1.4;
function setPxVariable(element, name, value) {
  if (!Number.isFinite(value)) return;
  element.style.setProperty(name, `${value}px`);
}

function workspaceFileImageDataUrl(path, file) {
  const mime = imageMimeType(path);
  const data = file?.data || file?.content || '';
  if (!data) return '';
  if (file?.encoding === 'base64') {
    return `data:${mime};base64,${String(data).replace(/\s/g, '')}`;
  }
  if (file?.encoding === 'utf8' && mime === 'image/svg+xml') {
    return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(data)}`;
  }
  return '';
}

function resolveWorkspaceAssetPath(markdownPath, rawSrc) {
  const value = String(rawSrc || '').trim();
  if (!value || /^(?:[a-z][a-z0-9+.-]*:|#)/i.test(value)) return '';
  const pathPart = value.split(/[?#]/, 1)[0];
  const decoded = safeDecodeUriComponent(pathPart);
  const parts = decoded.startsWith('/')
    ? []
    : parentWorkspacePath(markdownPath).split('/').filter(Boolean);
  decoded.split('/').forEach((part) => {
    if (!part || part === '.') return;
    if (part === '..') {
      parts.pop();
      return;
    }
    parts.push(part);
  });
  return normalizeWorkspacePath(parts.join('/'));
}

function safeDecodeUriComponent(value) {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

function filePathFromFileUri(value) {
  const raw = String(value || '').trim();
  if (!/^file:/i.test(raw)) return '';
  try {
    return decodeURIComponent(new URL(raw).pathname);
  } catch {
    return '';
  }
}

function normalizeAbsolutePath(value) {
  const raw = String(value || '').trim();
  if (!raw) return '';
  return raw.replace(/\\/g, '/').replace(/\/+$/, '');
}

function isAbsolutePath(value) {
  const raw = String(value || '').trim();
  return raw.startsWith('/') || /^[A-Za-z]:[\\/]/.test(raw);
}

function pathRelativeToRoot(path, root) {
  const absolutePath = normalizeAbsolutePath(path);
  const absoluteRoot = normalizeAbsolutePath(root);
  if (!absolutePath || !absoluteRoot) return '';
  if (absolutePath === absoluteRoot) return '';
  const prefix = `${absoluteRoot}/`;
  if (!absolutePath.startsWith(prefix)) return '';
  return normalizeWorkspacePath(absolutePath.slice(prefix.length));
}

function localAttachmentPath(attachment, rawUrl = '') {
  const uriPath = filePathFromFileUri(rawUrl)
    || filePathFromFileUri(attachment?.uri)
    || filePathFromFileUri(attachment?.file_uri)
    || filePathFromFileUri(attachment?.url);
  if (uriPath) return uriPath;
  return String(attachment?.path || attachment?.file_path || attachment?.src || '').trim();
}

function attachmentWorkspacePath(attachment, rawUrl, workspaceRoots = []) {
  const explicit = String(
    attachment?.workspace_path
    || attachment?.relative_path
    || attachment?.workspace_relative_path
    || ''
  ).trim();
  if (explicit) return normalizeWorkspacePath(explicit);
  const path = localAttachmentPath(attachment, rawUrl);
  if (!path) return '';
  if (!isAbsolutePath(path) && !/^file:/i.test(path) && !/^[a-z][a-z0-9+.-]*:/i.test(path)) {
    return normalizeWorkspacePath(path);
  }
  for (const root of workspaceRoots) {
    const relative = pathRelativeToRoot(path, root);
    if (relative) return relative;
  }
  return '';
}

function conversationFileTargetFromPath(value) {
  const path = normalizeAbsolutePath(value);
  const match = path.match(/(?:^|\/)conversations\/([^/]+)\/(.+)$/);
  if (!match) return null;
  const conversationId = match[1];
  const relativePath = normalizeWorkspacePath(match[2]);
  if (!conversationId || !relativePath) return null;
  return { conversationId, path: relativePath };
}

function workspaceTargetFromLocalLink(rawHref, fallbackConversationId, workspaceRoots = []) {
  const raw = String(rawHref || '').trim();
  if (!raw || raw.startsWith('#') || /^(?:https?:|mailto:|data:|blob:|javascript:)/i.test(raw)) {
    return null;
  }
  const withoutHash = raw.split('#', 1)[0];
  const withoutQuery = withoutHash.split('?', 1)[0];
  const decoded = safeDecodeUriComponent(withoutQuery);
  const localPath = filePathFromFileUri(decoded) || decoded;
  if (!localPath) return null;
  const explicitDirectory = /\/$/.test(localPath);
  const absoluteTarget = conversationFileTargetFromPath(localPath);
  if (absoluteTarget) {
    return { ...absoluteTarget, explicitDirectory };
  }
  if (isAbsolutePath(localPath)) {
    for (const root of workspaceRoots) {
      if (normalizeAbsolutePath(localPath) === normalizeAbsolutePath(root)) {
        return { conversationId: fallbackConversationId, path: '', explicitDirectory: true };
      }
      const relative = pathRelativeToRoot(localPath, root);
      if (relative) {
        return { conversationId: fallbackConversationId, path: relative, explicitDirectory };
      }
    }
    return null;
  }
  if (/^[a-z][a-z0-9+.-]*:/i.test(localPath)) return null;
  const path = normalizeWorkspacePath(localPath);
  if (!path || !fallbackConversationId) return null;
  return { conversationId: fallbackConversationId, path, explicitDirectory };
}

function attachmentConversationFileTarget(attachment, rawUrl, fallbackConversationId, workspaceRoots = []) {
  const absoluteTarget = conversationFileTargetFromPath(localAttachmentPath(attachment, rawUrl));
  if (absoluteTarget) return absoluteTarget;
  const path = attachmentWorkspacePath(attachment, rawUrl, workspaceRoots);
  if (!path || !fallbackConversationId) return null;
  return { conversationId: fallbackConversationId, path };
}

function attachmentCacheKey(serverId, conversationId, path, attachment, rawUrl = '') {
  return [
    serverId,
    conversationId,
    path,
    rawUrl,
    attachment?.uri,
    attachment?.file_uri,
    attachment?.path,
    attachment?.name
  ].map((value) => String(value || '')).join('|');
}

function applyChromeMetrics(metrics) {
  if (!metrics || typeof document === 'undefined') return;
  const root = document.documentElement;
  root.dataset.platform = metrics.platform || 'unknown';
  setPxVariable(root, '--window-controls-left-safe-area', metrics.leftSafeArea);
  setPxVariable(root, '--chrome-left-toolbar-offset', metrics.leftToolbarOffset);
  setPxVariable(root, '--chrome-title-left-offset', metrics.titleLeftOffset);
  setPxVariable(root, '--chrome-right-toolbar-offset', metrics.rightToolbarOffset);
  setPxVariable(root, '--chrome-title-right-offset', metrics.titleRightOffset);
  setPxVariable(root, '--chrome-title-right-offset-with-update', metrics.titleRightOffsetWithUpdate);
}

function composerModeInfo(status) {
  const remote = String(status?.remote || status?.tool_remote_mode || '').trim();
  const fixedRemote = remote.match(/^fixed ssh `([^`]*)` `([^`]*)`/);
  if (fixedRemote) {
    const host = fixedRemote[1];
    const cwd = fixedRemote[2];
    return {
      label: '远程',
      tone: 'remote',
      title: cwd ? `工具运行在 ${host}:${cwd}` : `工具运行在 ${host}`
    };
  }
  if (remote && remote !== 'selectable') {
    return { label: '远程', tone: 'remote', title: remote };
  }
  return {
    label: '本地',
    tone: 'local',
    title: '工具在当前 Stellaclaw 工作区执行'
  };
}

function measuredTerminalListMin(root) {
  const header = root?.querySelector('.terminal-list-header');
  const title = header?.querySelector('.terminal-title');
  const actions = header?.querySelector('.terminal-actions');
  if (!header || !title || !actions) return TERMINAL_LIST_MIN;
  const headerStyle = window.getComputedStyle(header);
  const padding = Number.parseFloat(headerStyle.paddingLeft || '0')
    + Number.parseFloat(headerStyle.paddingRight || '0');
  const gap = Number.parseFloat(headerStyle.columnGap || headerStyle.gap || '0');
  const measured = Math.ceil(title.scrollWidth + actions.scrollWidth + padding + gap + 8);
  return clamp(measured, TERMINAL_LIST_MIN, TERMINAL_LIST_MAX);
}

function layoutSnapshotFromValues(values = {}) {
  return {
    inspector: clamp(values.inspector, 320, 760) || 420,
    file: clamp(values.file, WORKSPACE_PANEL_MIN, WORKSPACE_PANEL_MAX) || 360,
    preview: clamp(values.preview, 320, 820) || 480,
    terminal: clamp(values.terminal, TERMINAL_HEIGHT_MIN, TERMINAL_HEIGHT_MAX) || 240,
    terminalList: clamp(values.terminalList, TERMINAL_LIST_MIN, TERMINAL_LIST_MAX) || 210
  };
}

function fileTabSnapshot(file) {
  const path = normalizeWorkspacePath(file?.path);
  if (!path) return null;
  return {
    path,
    name: file?.name || fileNameFromPath(path),
    kind: file?.kind || workspaceFileKind(path)
  };
}

class AppErrorBoundary extends React.Component {
  constructor(props) {
    super(props);
    this.state = { error: null };
  }

  static getDerivedStateFromError(error) {
    return { error };
  }

  componentDidCatch(error, info) {
    console.error('[StellaCodeX fatal render error]', error, info);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="app-fatal-error">
          <strong>界面渲染失败</strong>
          <span>{this.state.error?.message || '未知错误'}</span>
          <button type="button" onClick={() => window.location.reload()}>
            重新加载
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

function revokeFilePreviewUrls(files = []) {
  files.forEach((file) => {
    if (typeof file?.pdf_url === 'string' && file.pdf_url.startsWith('blob:')) {
      URL.revokeObjectURL(file.pdf_url);
    }
  });
}

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

function patchConversationForegroundSession(conversation, sessionId, patch) {
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

function nextForegroundSessionName(conversation) {
  const existingNames = new Set(
    foregroundSessions(conversation)
      .map((session) => displayForegroundSessionName(session, conversation).trim())
      .filter(Boolean)
  );
  let index = Math.max(2, existingNames.size + 1);
  while (existingNames.has(`Session ${index}`)) index += 1;
  return `Session ${index}`;
}

function createLocalForegroundSessionId(conversation) {
  const existingIds = new Set(foregroundSessions(conversation).map((session) => String(session?.id || 'main')));
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const suffix = Math.random().toString(36).slice(2, 8);
    const id = `session_${Date.now().toString(36)}_${suffix}`;
    if (!existingIds.has(id)) return id;
  }
  return `session_${Date.now().toString(36)}`;
}

function applyConversationStreamEvent(current, payload) {
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

function hasUnreadConversation(conversation) {
  return foregroundSessions(conversation).some((session) => (
    compareMessageIds(session?.last_message_id, session?.last_seen_message_id) > 0
  ));
}

function slashCommandState(value) {
  const command = String(value || '').trim();
  const name = command.split(/\s+/, 1)[0]?.toLowerCase() || '';
  if (name === '/model') {
    return { control: true, name, title: '切换模型', detail: '模型切换命令已发送' };
  }
  if (name === '/remote') {
    return { control: true, name, title: '切换远程模式', detail: '远程模式命令已发送' };
  }
  if (name === '/reasoning') {
    return { control: true, name, title: '切换推理强度', detail: 'reasoning effort 命令已发送' };
  }
  if (name === '/idle_compact' || name === '/idle_timeout_compact') {
    return { control: true, name, title: '切换空闲压缩', detail: 'idle compact 命令已发送' };
  }
  if (name === '/cancel') {
    return { control: true, name, title: '取消执行', detail: '取消命令已发送' };
  }
  if (name === '/compact') {
    return { control: true, name, title: '压缩上下文', detail: '压缩命令已发送' };
  }
  if (name === '/status') {
    return { control: true, name, title: '读取状态', detail: '状态命令已发送' };
  }
  return { control: false, name, title: '等待响应', detail: '消息已送达，等待模型开始处理' };
}

function controlCommandActivity(command) {
  const name = String(command || '').trim().split(/\s+/, 1)[0]?.toLowerCase() || '';
  if (name === '/model') return '模型切换命令已发送';
  if (name === '/reasoning') return 'Reasoning 命令已发送';
  if (name === '/idle_compact' || name === '/idle_timeout_compact') return '空闲压缩设置命令已发送';
  return 'Conversation 设置命令已发送';
}

function App() {
  const [settings, setSettings] = useState(null);
  const [systemTheme, setSystemTheme] = useState(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return 'dark';
    return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  });
  const [sidebarMode, setSidebarMode] = useState('expanded');
  const [activeServerId, setActiveServerId] = useState('');
  const [conversations, setConversations] = useState([]);
  const [statuses, setStatuses] = useState(new Map());
  const [selected, setSelected] = useState(null);
  const [messages, setMessages] = useState([]);
  const [messagesReady, setMessagesReady] = useState(false);
  const [loading, setLoading] = useState(false);
  const [sending, setSending] = useState(false);
  const [sessionActivity, setSessionActivity] = useState('');
  const [chatSessionState, setChatSessionState] = useState({ state: 'idle' });
  const [runningActivities, setRunningActivities] = useState([]);
  const [overviewPanelOpen, setOverviewPanelOpen] = useState(false);
  const [workspacePanelOpen, setWorkspacePanelOpen] = useState(false);
  const [previewPanelOpen, setPreviewPanelOpen] = useState(false);
  const [statusDeltas, setStatusDeltas] = useState(() => new Map());
  const [workspaceListings, setWorkspaceListings] = useState(() => new Map());
  const [workspaceExpanded, setWorkspaceExpanded] = useState(() => new Set(['']));
  const [workspaceLoading, setWorkspaceLoading] = useState(() => new Set());
  const [workspaceError, setWorkspaceError] = useState('');
  const [transfers, setTransfers] = useState([]);
  const [updaterStatus, setUpdaterStatus] = useState({ state: 'idle' });
  const [terminalOpen, setTerminalOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsSaving, setSettingsSaving] = useState(false);
  const [conversationLayout, setConversationLayout] = useState(null);
  const [newConversationOpen, setNewConversationOpen] = useState(false);
  const [creatingConversation, setCreatingConversation] = useState(false);
  const [renamingConversation, setRenamingConversation] = useState(null);
  const [renamingConversationSaving, setRenamingConversationSaving] = useState(false);
  const [renamingSession, setRenamingSession] = useState(null);
  const [renamingSessionSaving, setRenamingSessionSaving] = useState(false);
  const [propertiesConversation, setPropertiesConversation] = useState(null);
  const [propertiesModels, setPropertiesModels] = useState([]);
  const [propertiesModelsLoading, setPropertiesModelsLoading] = useState(false);
  const [propertiesModelsError, setPropertiesModelsError] = useState('');
  const [propertiesApplying, setPropertiesApplying] = useState(false);
  const [openFiles, setOpenFiles] = useState([]);
  const [activeFilePath, setActiveFilePath] = useState('');
  const [selectionReferences, setSelectionReferences] = useState([]);
  const [appForeground, setAppForeground] = useState(() => (
    typeof document === 'undefined'
      ? true
      : document.visibilityState === 'visible' && document.hasFocus()
  ));
  const messagesRef = useRef([]);
  const conversationsRef = useRef([]);
  const openFilesRef = useRef([]);
  const appForegroundRef = useRef(appForeground);
  const settingsRef = useRef(null);
  const settingsSaveSeqRef = useRef(0);
  const selectedRef = useRef(null);
  const websocketKeyRef = useRef('');
  const seenUsageMessagesRef = useRef(new Map());
  const attachmentImageUrlCacheRef = useRef(new Map());
  const workspaceResourceCacheRef = useRef(new Map());
  const loadingOlderRef = useRef(false);
  const layoutDraftRef = useRef(null);
  const restoringUiRef = useRef(false);
  const uiSaveTimerRef = useRef(null);
  const readSaveTimersRef = useRef(new Map());
  const selectedSessionId = selectedForegroundSessionId(selected);
  const selectedServerId = selected?.serverId || '';
  const selectedConversationId = selected?.conversationId || '';

  useEffect(() => {
    messagesRef.current = messages;
  }, [messages]);

  useEffect(() => {
    conversationsRef.current = conversations;
  }, [conversations]);

  useEffect(() => {
    appForegroundRef.current = appForeground;
  }, [appForeground]);

  useEffect(() => {
    settingsRef.current = settings;
  }, [settings]);

  useLayoutEffect(() => {
    applyChromeMetrics(window.stellacode2?.chromeMetrics?.());
  }, []);

  useEffect(() => {
    let frame = 0;
    let resolutionQuery = null;
    let removeResolutionListener = null;
    const refreshChromeMetrics = () => {
      if (frame) window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(() => {
        frame = 0;
        applyChromeMetrics(window.stellacode2?.chromeMetrics?.());
      });
    };
    const bindResolutionListener = () => {
      removeResolutionListener?.();
      if (!window.matchMedia) return;
      resolutionQuery = window.matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
      const listener = () => {
        refreshChromeMetrics();
        bindResolutionListener();
      };
      resolutionQuery.addEventListener?.('change', listener);
      removeResolutionListener = () => resolutionQuery?.removeEventListener?.('change', listener);
    };
    window.addEventListener('resize', refreshChromeMetrics);
    bindResolutionListener();
    return () => {
      if (frame) window.cancelAnimationFrame(frame);
      window.removeEventListener('resize', refreshChromeMetrics);
      removeResolutionListener?.();
    };
  }, []);

  useEffect(() => {
    const scale = clamp(settings?.uiScale, MIN_UI_SCALE, MAX_UI_SCALE) || 1;
    window.stellacode2?.setZoomFactor?.(scale).catch(() => {});
  }, [settings?.uiScale]);

  useEffect(() => {
    const updateForeground = () => {
      setAppForeground(document.visibilityState === 'visible' && document.hasFocus());
    };
    updateForeground();
    window.addEventListener('focus', updateForeground);
    window.addEventListener('blur', updateForeground);
    document.addEventListener('visibilitychange', updateForeground);
    return () => {
      window.removeEventListener('focus', updateForeground);
      window.removeEventListener('blur', updateForeground);
      document.removeEventListener('visibilitychange', updateForeground);
    };
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = settings?.themeMode || 'system';
  }, [settings?.themeMode]);

  useEffect(() => {
    if (!window.matchMedia) return undefined;
    const query = window.matchMedia('(prefers-color-scheme: light)');
    const apply = () => setSystemTheme(query.matches ? 'light' : 'dark');
    apply();
    query.addEventListener?.('change', apply);
    return () => query.removeEventListener?.('change', apply);
  }, []);

  const activeThemeMode = effectiveThemeMode(settings?.themeMode, systemTheme);
  const themeVariables = useMemo(
    () => themeCssVariables(settings?.themeColors, activeThemeMode),
    [activeThemeMode, settings?.themeColors]
  );

  useEffect(() => {
    const root = document.documentElement;
    Object.entries(themeVariables).forEach(([name, value]) => {
      root.style.setProperty(name, value);
    });
  }, [themeVariables]);

  useEffect(() => {
    selectedRef.current = selected;
  }, [selected]);

  useEffect(() => {
    openFilesRef.current = openFiles;
  }, [openFiles]);

  useEffect(() => {
    setSelectionReferences([]);
  }, [selected?.serverId, selected?.conversationId, selectedSessionId]);

  useEffect(() => () => {
    revokeFilePreviewUrls(openFilesRef.current);
  }, []);

  useEffect(() => {
    const updater = window.stellacode2?.updater;
    if (!updater) return undefined;
    let disposed = false;
    const applyStatus = (status) => {
      if (!disposed && status) {
        setUpdaterStatus(status);
      }
    };
    updater.status?.().then(applyStatus).catch(() => {});
    const unsubscribe = updater.onStatus?.(applyStatus);
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, []);

  const globalLayoutValues = settings?.layout || {};
  const conversationLayoutValues = conversationLayout || globalLayoutValues;
  const sidebarWidth = sidebarMode === 'collapsed' ? SIDEBAR_COLLAPSED : clamp(globalLayoutValues.sidebar, 220, 520) || SIDEBAR_EXPANDED;
  const overviewPanelWidth = clamp(conversationLayoutValues.inspector, 320, 760) || 420;
  const workspacePanelWidth = clamp(conversationLayoutValues.file, WORKSPACE_PANEL_MIN, WORKSPACE_PANEL_MAX) || 360;
  const previewPanelWidth = clamp(conversationLayoutValues.preview, 320, 820) || 480;
  const terminalHeight = clamp(conversationLayoutValues.terminal, TERMINAL_HEIGHT_MIN, TERMINAL_HEIGHT_MAX) || 240;
  const terminalListWidth = clamp(conversationLayoutValues.terminalList, TERMINAL_LIST_MIN, TERMINAL_LIST_MAX) || 210;
  const previewPanelRight = workspacePanelOpen ? workspacePanelWidth : 0;
  const overviewPanelRight = previewPanelRight + (previewPanelOpen ? previewPanelWidth : 0);
  const rightContentInset = (overviewPanelOpen ? overviewPanelWidth : 0) + (workspacePanelOpen ? workspacePanelWidth : 0) + (previewPanelOpen ? previewPanelWidth : 0);
  const activeConversation = useMemo(
    () => conversations.find((item) => item.conversation_id === selected?.conversationId) || null,
    [conversations, selected]
  );
  const propertiesConversationCurrent = useMemo(() => (
    propertiesConversation
      ? conversations.find((item) => item.conversation_id === propertiesConversation.conversation_id) || propertiesConversation
      : null
  ), [conversations, propertiesConversation]);
  const activeForegroundSession = useMemo(() => {
    if (!activeConversation) return null;
    return foregroundSessions(activeConversation).find((session) => (
      String(session?.id || 'main') === selectedSessionId
    )) || foregroundSessions(activeConversation)[0] || null;
  }, [activeConversation, selectedSessionId]);
  const displayFontSize = clamp(settings?.displayFontSize, MIN_DISPLAY_FONT_SIZE, MAX_DISPLAY_FONT_SIZE) || 12;
  const uiScale = clamp(settings?.uiScale, MIN_UI_SCALE, MAX_UI_SCALE) || 1;
  const terminalFontSize = clamp(displayFontSize + 1, 11, 22) || 13;
  const selectedKey = selected ? conversationKey(selected.serverId, selected.conversationId, selectedSessionId) : '';
  const selectedConversationUiKey = selected ? conversationKey(selected.serverId, selected.conversationId, 'main') : '';
  const selectedStatus = selected ? statuses.get(selectedKey) : null;
  const selectedChatSessionState = chatSessionState.scopeKey === selectedKey
    ? chatSessionState
    : { state: 'idle' };
  const selectedConversationStatus = useMemo(() => ({
    ...(selectedStatus || {}),
    ...(activeConversation ? {
      model: activeConversation.model,
      model_selection_pending: activeConversation.model_selection_pending,
      reasoning: activeConversation.reasoning,
      sandbox: activeConversation.sandbox,
      sandbox_source: activeConversation.sandbox_source,
      remote: activeConversation.remote,
      workspace: activeConversation.workspace,
      processing_state: selectedChatSessionState.state || 'idle',
      running: chatSessionStateIsActive(selectedChatSessionState),
      running_background: activeConversation.running_background,
      total_background: activeConversation.total_background,
      running_subagents: activeConversation.running_subagents,
      total_subagents: activeConversation.total_subagents
    } : {})
  }), [selectedStatus, activeConversation, selectedChatSessionState]);
  const activeServer = useMemo(
    () => (settings?.servers || []).find((server) => server.id === activeServerId) || null,
    [settings?.servers, activeServerId]
  );
  const activeUserName = String(activeServer?.userName || '').trim() || 'workspace-user';
  const settingsReady = Boolean(settings);
  const composerMode = useMemo(
    () => composerModeInfo(selectedConversationStatus),
    [selectedConversationStatus]
  );
  const messageAttachmentWorkspaceRoots = useMemo(() => {
    const rootListing = workspaceListings.get('');
    return Array.from(new Set([
      workspaceDisplayRoot(rootListing, selectedConversationStatus),
      rootListing?.workspace_root,
      rootListing?.remote?.cwd,
      selectedConversationStatus?.workspace,
      activeConversation?.workspace
    ].map(normalizeAbsolutePath).filter(Boolean)));
  }, [workspaceListings, selectedConversationStatus, activeConversation?.workspace]);
  const readWorkspaceResourceCache = useCallback((kind, parts) => {
    const key = localCacheKey(kind, parts);
    if (workspaceResourceCacheRef.current.has(key)) {
      return workspaceResourceCacheRef.current.get(key);
    }
    const cached = readLocalCache(kind, parts);
    if (cached !== null) {
      workspaceResourceCacheRef.current.set(key, cached);
    }
    return cached;
  }, []);
  const writeWorkspaceResourceCache = useCallback((kind, parts, value) => {
    const key = localCacheKey(kind, parts);
    workspaceResourceCacheRef.current.set(key, value);
    writeLocalCache(kind, parts, value);
  }, []);
  const removeWorkspaceResourceCache = useCallback((kind, parts) => {
    const key = localCacheKey(kind, parts);
    workspaceResourceCacheRef.current.delete(key);
    removeLocalCache(kind, parts);
  }, []);
  const loadWorkspaceFileCached = useCallback(async (serverId, conversationId, path, limitBytes, options = {}) => {
    const normalized = normalizeWorkspacePath(path);
    const parts = [serverId, conversationId, normalized, limitBytes || 'full'];
    const useCache = options.cache !== false;
    if (useCache && !options.force) {
      const cached = readWorkspaceResourceCache('workspace-file', parts);
      if (cached) return cached;
    }
    const loaded = await loadWorkspaceFile(serverId, conversationId, normalized, limitBytes);
    if (useCache) {
      writeWorkspaceResourceCache('workspace-file', parts, loaded);
    }
    return loaded;
  }, [readWorkspaceResourceCache, writeWorkspaceResourceCache]);
  const resolveMessageAttachmentUrl = useCallback(async (attachment, rawUrl = '') => {
    const serverId = selected?.serverId || '';
    const conversationId = selected?.conversationId || '';
    if (!serverId || !conversationId) return rawUrl || '';
    const target = attachmentConversationFileTarget(attachment, rawUrl, conversationId, messageAttachmentWorkspaceRoots);
    if (!target) return rawUrl || '';
    const key = attachmentCacheKey(serverId, target.conversationId, target.path, attachment, rawUrl);
    if (attachmentImageUrlCacheRef.current.has(key)) {
      return attachmentImageUrlCacheRef.current.get(key);
    }
    try {
      const file = await loadWorkspaceFileCached(serverId, target.conversationId, target.path, MESSAGE_IMAGE_PREVIEW_MAX_BYTES);
      const dataUrl = workspaceFileImageDataUrl(target.path || attachmentName(attachment), file);
      if (dataUrl) {
        attachmentImageUrlCacheRef.current.set(key, dataUrl);
        return dataUrl;
      }
    } catch {
      return rawUrl || '';
    }
    return rawUrl || '';
  }, [selected?.serverId, selected?.conversationId, messageAttachmentWorkspaceRoots, loadWorkspaceFileCached]);
  const messageAttachmentWorkspaceTarget = useCallback((attachment, rawUrl = '') => (
    attachmentConversationFileTarget(attachment, rawUrl, selected?.conversationId || '', messageAttachmentWorkspaceRoots)
  ), [messageAttachmentWorkspaceRoots, selected?.conversationId]);
  const messageAttachmentWorkspacePath = useCallback((attachment) => (
    messageAttachmentWorkspaceTarget(attachment)?.path
    || String(attachment?.path || '').trim()
  ), [messageAttachmentWorkspaceTarget]);
  const selectedUsage = useMemo(
    () => statusUsageTotals(selectedStatus, selectedKey ? statusDeltas.get(selectedKey) : null),
    [selectedStatus, selectedKey, statusDeltas]
  );
  const selectedProcessingState = String(selectedConversationStatus?.processing_state || '').trim().toLowerCase();
  const selectedProcessing = Boolean(selectedConversationStatus?.running)
    || isActiveSessionState(selectedProcessingState);
  const updateReady = updaterStatus?.state === 'downloaded';

  const upsertTransfer = useCallback((id, patch) => {
    setTransfers((current) => {
      const existing = current.find((item) => item.id === id);
      const nextItem = { ...(existing || { id, createdAt: Date.now() }), ...patch };
      if (!existing) return [nextItem, ...current].slice(0, 5);
      return current.map((item) => (item.id === id ? nextItem : item));
    });
  }, []);

  const finishTransfer = useCallback((id, patch) => {
    upsertTransfer(id, { ...patch, done: true });
    window.setTimeout(() => {
      setTransfers((current) => current.filter((item) => item.id !== id));
    }, 3200);
  }, [upsertTransfer]);

  const updateRunningActivities = useCallback((updater) => {
    setRunningActivities((current) => {
      const next = updater(current).slice(-5);
      return liveActivitySignature(next) === liveActivitySignature(current) ? current : next;
    });
  }, []);

  const saveSettings = useCallback(async (next) => {
    const base = settingsRef.current || {};
    const request = {
      ...base,
      ...(next || {}),
      layout: next?.layout ? { ...(base.layout || {}), ...(next.layout || {}) } : base.layout,
      conversationUi: next?.conversationUi ? { ...(base.conversationUi || {}), ...(next.conversationUi || {}) } : base.conversationUi,
      conversationListUi: next?.conversationListUi ? { ...(base.conversationListUi || {}), ...(next.conversationListUi || {}) } : base.conversationListUi,
      hiddenConversations: next?.hiddenConversations ?? base.hiddenConversations
    };
    const seq = settingsSaveSeqRef.current + 1;
    settingsSaveSeqRef.current = seq;
    const saved = await window.stellacode2.saveSettings(request);
    const merged = {
      ...saved,
      layout: request.layout ? { ...(saved.layout || {}), ...(request.layout || {}) } : saved.layout,
      conversationUi: request.conversationUi ? { ...(saved.conversationUi || {}), ...(request.conversationUi || {}) } : saved.conversationUi,
      conversationListUi: request.conversationListUi ? { ...(saved.conversationListUi || {}), ...(request.conversationListUi || {}) } : saved.conversationListUi,
      hiddenConversations: request.hiddenConversations ?? saved.hiddenConversations
    };
    if (settingsSaveSeqRef.current !== seq) {
      const latest = settingsRef.current;
      if (latest) window.stellacode2.saveSettings(latest).catch(() => {});
      return latest || merged;
    }
    setSettings(merged);
    settingsRef.current = merged;
    return merged;
  }, []);

  const queueConversationUiSave = useCallback((key, snapshot) => {
    if (!key || !snapshot) return;
    setSettings((current) => {
      if (!current) return current;
      const next = {
        ...current,
        conversationUi: {
          ...(current.conversationUi || {}),
          [key]: snapshot
        }
      };
      window.clearTimeout(uiSaveTimerRef.current);
      uiSaveTimerRef.current = window.setTimeout(() => {
        window.stellacode2.saveSettings(settingsRef.current || next).catch(() => {});
      }, 260);
      return next;
    });
  }, []);

  const markConversationRead = useCallback((serverId, conversationId, foregroundSessionId, lastMessageId) => {
    if (!appForegroundRef.current) return;
    const seen = String(lastMessageId || '').trim();
    if (!serverId || !conversationId || !seen || messageOrderFromId(seen) === undefined) return;
    const sessionId = foregroundSessionId || 'main';
    const key = conversationKey(serverId, conversationId, sessionId);
    setConversations((current) => {
      const next = current.map((conversation) => (
        conversation.conversation_id === conversationId
          ? patchConversationForegroundSession(conversation, sessionId, {
            last_seen_message_id: seen
          })
          : conversation
      ));
      conversationsRef.current = next;
      return next;
    });
    const existing = readSaveTimersRef.current.get(key);
    if (existing) window.clearTimeout(existing);
    const timer = window.setTimeout(() => {
      readSaveTimersRef.current.delete(key);
      markConversationSeen(serverId, conversationId, seen, sessionId).catch(() => {});
    }, 180);
    readSaveTimersRef.current.set(key, timer);
  }, []);

  useEffect(() => () => {
    window.clearTimeout(uiSaveTimerRef.current);
    for (const timer of readSaveTimersRef.current.values()) {
      window.clearTimeout(timer);
    }
    readSaveTimersRef.current.clear();
  }, []);

  const refreshConversations = useCallback(async (serverId) => {
    if (!serverId) return;
    setLoading(true);
    try {
      const list = await loadConversations(serverId);
      setConversations(list);
      if (!selectedRef.current && list[0]) {
        const sessionId = foregroundSessions(list[0])[0]?.id || 'main';
        setSelected({ serverId, conversationId: list[0].conversation_id, foregroundSessionId: sessionId });
      }
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!activeServerId || !settingsReady) return undefined;
    let disposed = false;
    let reconnectTimer = null;
    let streamSocket = null;
    const connect = async () => {
      try {
        const url = await conversationStreamUrl(activeServerId);
        if (disposed) return;
        const socket = new WebSocket(url);
        streamSocket = socket;
        socket.addEventListener('message', (event) => {
          let payload;
          try {
            payload = JSON.parse(event.data);
          } catch {
            return;
          }
          const nextConversations = applyConversationStreamEvent(conversationsRef.current, payload);
          conversationsRef.current = nextConversations;
          setConversations(nextConversations);
          const homeType = String(payload?.type || '').startsWith('home.')
            ? String(payload.type).slice('home.'.length)
            : String(payload?.type || '');
          if (
            homeType === 'conversation_deleted'
            && selectedRef.current?.serverId === activeServerId
            && selectedRef.current?.conversationId === payload.conversation_id
          ) {
            const next = nextConversations[0];
            const sessionId = next ? foregroundSessions(next)[0]?.id || 'main' : 'main';
            setSelected(next ? { serverId: activeServerId, conversationId: next.conversation_id, foregroundSessionId: sessionId } : null);
          }
          if (!selectedRef.current) {
            const fallbackConversation = (homeType === 'snapshot' || homeType === 'conversation_snapshot')
              ? (payload.conversations || [])[0]
              : payload.conversation;
            if (fallbackConversation?.conversation_id) {
              const sessionId = foregroundSessions(fallbackConversation)[0]?.id || 'main';
              setSelected({ serverId: activeServerId, conversationId: fallbackConversation.conversation_id, foregroundSessionId: sessionId });
            }
          }
          if (homeType === 'conversation_turn_completed' && payload.conversation_id) {
            const completed = nextConversations.find((conversation) => conversation.conversation_id === payload.conversation_id);
            const selectedConversation = selectedRef.current;
            const isActive = selectedConversation?.serverId === activeServerId
              && selectedConversation?.conversationId === payload.conversation_id;
            const isVisibleActive = isActive && appForegroundRef.current;
            if (selectedConversation && completed && !isVisibleActive && hasUnreadConversation(completed)) {
              window.stellacode2?.notify?.({
                title: displayConversationName(completed),
                body: '新回复已完成'
              }).catch(() => {});
            }
          }
        });
        socket.addEventListener('close', () => {
          if (disposed) return;
          reconnectTimer = window.setTimeout(connect, 1600);
        });
        socket.addEventListener('error', () => {});
      } catch {
        if (disposed) return;
        reconnectTimer = window.setTimeout(connect, 2400);
      }
    };
    connect();
    return () => {
      disposed = true;
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      if (streamSocket && streamSocket.readyState <= WebSocket.OPEN) streamSocket.close();
    };
  }, [activeServerId, settingsReady]);

  useEffect(() => {
    if (!appForeground) return;
    if (!selectedKey || !activeForegroundSession?.last_message_id) return;
    markConversationRead(selected.serverId, selected.conversationId, selectedSessionId, activeForegroundSession.last_message_id);
  }, [appForeground, selected, selectedKey, selectedSessionId, activeForegroundSession?.last_message_id, markConversationRead]);

  const saveSettingsFromDialog = useCallback(async (next) => {
    setSettingsSaving(true);
    try {
      const saved = await saveSettings(next);
      if (saved.activeServerId !== activeServerId) {
        selectedRef.current = null;
        setSelected(null);
      }
      setActiveServerId(saved.activeServerId);
      setSettingsOpen(false);
      await refreshConversations(saved.activeServerId);
    } catch (error) {
      window.alert(error?.message || '保存设置失败');
    } finally {
      setSettingsSaving(false);
    }
  }, [activeServerId, refreshConversations, saveSettings]);

  useEffect(() => {
    window.stellacode2.loadSettings().then((loaded) => {
      setSettings(loaded);
      setSidebarMode(loaded.sidebarMode || 'expanded');
      setActiveServerId(loaded.activeServerId);
      refreshConversations(loaded.activeServerId);
    });
  }, [refreshConversations]);

  const openRenameConversationDialog = useCallback((conversation) => {
    if (!conversation) return;
    setRenamingConversation(conversation);
  }, []);

  const renameSelectedConversation = useCallback(async (conversation, nickname) => {
    if (!activeServerId || !conversation) return;
    const currentName = displayConversationName(conversation);
    const nextName = String(nickname || '').trim();
    if (!nextName || nextName === currentName) {
      setRenamingConversation(null);
      return;
    }
    setRenamingConversationSaving(true);
    try {
      const updated = await renameConversation(activeServerId, conversation.conversation_id, nextName);
      setConversations((current) => current.map((item) => (
        item.conversation_id === conversation.conversation_id
          ? { ...item, ...(updated || {}), nickname: nextName }
          : item
      )));
      setRenamingConversation(null);
    } catch (error) {
      window.alert(error?.message || '重命名失败');
    } finally {
      setRenamingConversationSaving(false);
    }
  }, [activeServerId]);

  const deleteSelectedConversation = useCallback(async (conversation) => {
    if (!activeServerId || !conversation || !settings) return;
    const title = displayConversationName(conversation);
    if (!window.confirm(`删除 Conversation「${title}」？`)) return;
    try {
      await deleteConversation(activeServerId, conversation.conversation_id);
      setConversations((current) => {
        const next = current.filter((item) => item.conversation_id !== conversation.conversation_id);
        if (selected?.conversationId === conversation.conversation_id) {
          const sessionId = next[0] ? foregroundSessions(next[0])[0]?.id || 'main' : 'main';
          setSelected(next[0] ? { serverId: activeServerId, conversationId: next[0].conversation_id, foregroundSessionId: sessionId } : null);
        }
        return next;
      });
    } catch (error) {
      window.alert(error?.message || '删除 Conversation 失败');
    }
  }, [activeServerId, selected?.conversationId, settings]);

  const setConversationHidden = useCallback(async (conversation, hidden) => {
    if (!activeServerId || !conversation || !settings) return;
    const conversationId = conversation.conversation_id;
    const currentIds = new Set((settings.hiddenConversations?.[activeServerId] || []).map(String));
    if (hidden) {
      currentIds.add(conversationId);
    } else {
      currentIds.delete(conversationId);
    }
    const nextHiddenConversations = {
      ...(settings.hiddenConversations || {}),
      [activeServerId]: Array.from(currentIds)
    };
    if (nextHiddenConversations[activeServerId].length === 0) {
      delete nextHiddenConversations[activeServerId];
    }
    const nextSettings = {
      ...settings,
      hiddenConversations: nextHiddenConversations
    };
    setSettings(nextSettings);
    if (hidden && selected?.conversationId === conversationId) {
      const nextVisible = conversations.find((item) => (
        item.conversation_id !== conversationId
          && !currentIds.has(item.conversation_id)
      ));
      const sessionId = nextVisible ? foregroundSessions(nextVisible)[0]?.id || 'main' : 'main';
      setSelected(nextVisible ? { serverId: activeServerId, conversationId: nextVisible.conversation_id, foregroundSessionId: sessionId } : null);
    }
    try {
      await saveSettings(nextSettings);
    } catch (error) {
      window.alert(error?.message || '保存隐藏状态失败');
    }
  }, [activeServerId, conversations, saveSettings, selected?.conversationId, settings]);

  const updateConversationListUi = useCallback((patch) => {
    if (!activeServerId || !settings) return;
    const currentListUi = settings.conversationListUi?.[activeServerId] || {};
    const nextServerListUi = {
      ...currentListUi,
      ...patch
    };
    const nextSettings = {
      ...settings,
      conversationListUi: {
        ...(settings.conversationListUi || {}),
        [activeServerId]: nextServerListUi
      }
    };
    setSettings(nextSettings);
    saveSettings(nextSettings).catch(() => {});
  }, [activeServerId, saveSettings, settings]);

  const createNewConversation = useCallback(async ({ serverId, nickname }) => {
    if (!serverId || creatingConversation) return;
    setCreatingConversation(true);
    try {
      const response = await createConversation(serverId, { nickname });
      const conversationId = response?.conversation_id;
      if (!conversationId) throw new Error('创建 Conversation 失败');
      if (settings?.activeServerId !== serverId) {
        await saveSettings({ ...(settings || {}), activeServerId: serverId });
        setActiveServerId(serverId);
      }
      const list = await loadConversations(serverId);
      setConversations(list);
      setSelected({ serverId, conversationId, foregroundSessionId: 'main' });
      setNewConversationOpen(false);
      setOverviewPanelOpen(false);
      setWorkspacePanelOpen(false);
      setPreviewPanelOpen(false);
    } catch (error) {
      window.alert(error?.message || '创建 Conversation 失败');
    } finally {
      setCreatingConversation(false);
    }
  }, [creatingConversation, saveSettings, settings]);

  const createConversationForegroundSession = useCallback(async (conversation) => {
    if (!activeServerId || !conversation) return;
    const sessionId = createLocalForegroundSessionId(conversation);
    const nickname = nextForegroundSessionName(conversation);
    try {
      const session = await createForegroundSession(activeServerId, conversation.conversation_id, {
        sessionId,
        nickname
      });
      const createdSessionId = session?.id || sessionId;
      const list = await loadConversations(activeServerId);
      setConversations(list);
      setSelected({
        serverId: activeServerId,
        conversationId: conversation.conversation_id,
        foregroundSessionId: createdSessionId
      });
      updateConversationListUi({
        openConversationIds: Array.from(new Set([
          ...((settings?.conversationListUi?.[activeServerId]?.openConversationIds) || []),
          conversation.conversation_id
        ]))
      });
    } catch (error) {
      window.alert(error?.message || '创建对话失败');
    }
  }, [activeServerId, settings, updateConversationListUi]);

  const renameConversationForegroundSession = useCallback(async (conversation, session) => {
    if (!activeServerId || !conversation || !session) return;
    setRenamingSession({ conversation, session });
  }, [activeServerId]);

  const renameSelectedForegroundSession = useCallback(async (conversation, session, nickname) => {
    if (!activeServerId || !conversation || !session) return;
    const sessionId = session.id || 'main';
    const currentName = displayForegroundSessionName(session, conversation).trim();
    const nextName = String(nickname || '').trim();
    if (!nextName || nextName === currentName) {
      setRenamingSession(null);
      return;
    }
    setRenamingSessionSaving(true);
    try {
      await renameForegroundSession(activeServerId, conversation.conversation_id, sessionId, nextName);
      const list = await loadConversations(activeServerId);
      setConversations(list);
      setRenamingSession(null);
    } catch (error) {
      window.alert(error?.message || '重命名 Session 失败');
    } finally {
      setRenamingSessionSaving(false);
    }
  }, [activeServerId]);

  const deleteConversationForegroundSession = useCallback(async (conversation, session) => {
    if (!activeServerId || !conversation || !session || session.is_main) return;
    const title = displayForegroundSessionName(session, conversation);
    if (!window.confirm(`删除对话「${title}」？`)) return;
    const sessionId = session.id || 'main';
    try {
      await deleteForegroundSession(activeServerId, conversation.conversation_id, sessionId);
      const list = await loadConversations(activeServerId);
      setConversations(list);
      if (
        selected?.conversationId === conversation.conversation_id
        && selectedSessionId === sessionId
      ) {
        const refreshedConversation = list.find((item) => item.conversation_id === conversation.conversation_id);
        const fallback = foregroundSessions(refreshedConversation)[0]?.id || 'main';
        setSelected(refreshedConversation
          ? { serverId: activeServerId, conversationId: conversation.conversation_id, foregroundSessionId: fallback }
          : list[0]
            ? { serverId: activeServerId, conversationId: list[0].conversation_id, foregroundSessionId: foregroundSessions(list[0])[0]?.id || 'main' }
            : null);
      }
    } catch (error) {
      window.alert(error?.message || '删除对话失败');
    }
  }, [activeServerId, selected?.conversationId, selectedSessionId]);

  const fetchWorkspacePath = useCallback(async (path = '', options = {}) => {
    if (!selected) return null;
    const normalized = normalizeWorkspacePath(path);
    if (!options.force && workspaceListings.has(normalized)) {
      return workspaceListings.get(normalized);
    }
    const cacheParts = [selected.serverId, selected.conversationId, normalized, 500];
    if (!options.force) {
      const cached = readWorkspaceResourceCache('workspace-listing', cacheParts);
      if (cached) {
        setWorkspaceListings((current) => (
          current.has(normalized) ? current : new Map(current).set(normalized, cached)
        ));
        return cached;
      }
    }
    setWorkspaceError('');
    setWorkspaceLoading((current) => new Set(current).add(normalized));
    try {
      const listing = await loadWorkspace(selected.serverId, selected.conversationId, normalized, 500);
      writeWorkspaceResourceCache('workspace-listing', cacheParts, listing);
      setWorkspaceListings((current) => new Map(current).set(normalized, listing));
      return listing;
    } catch (error) {
      setWorkspaceError(error?.message || '读取工作区失败');
      throw error;
    } finally {
      setWorkspaceLoading((current) => {
        const next = new Set(current);
        next.delete(normalized);
        return next;
      });
    }
  }, [selected, workspaceListings, readWorkspaceResourceCache, writeWorkspaceResourceCache]);

  const loadPdfPreviewIntoTab = useCallback(async (entry, options = {}) => {
    if (!selected || !entry) return;
    const path = normalizeWorkspacePath(entry.path);
    const serverId = selected.serverId;
    const selectedConversationId = selected.conversationId;
    const conversationId = String(entry.conversationId || entry.conversation_id || selectedConversationId || '').trim();
    if (!conversationId) return;
    if (!options.keepExistingPreview) {
      setOpenFiles((current) => current.map((item) => (
        item.path === path ? { ...item, loading: true, error: '' } : item
      )));
    }
    try {
      const preview = await window.stellacode2.previewWorkspace({
        serverId,
        conversationId,
        path,
        kind: 'file',
        mediaType: 'application/pdf',
        maxBytes: PDF_PREVIEW_MAX_BYTES,
        suggestedName: entry.name || fileNameFromPath(path)
      });
      const blob = new Blob([preview.data], { type: preview.mediaType || 'application/pdf' });
      const pdfUrl = URL.createObjectURL(blob);
      if (
        selectedRef.current?.serverId !== serverId
        || selectedRef.current?.conversationId !== selectedConversationId
      ) {
        URL.revokeObjectURL(pdfUrl);
        return;
      }
      setOpenFiles((current) => {
        const existing = current.find((item) => item.path === path);
        if (!existing) {
          URL.revokeObjectURL(pdfUrl);
          return current;
        }
        revokeFilePreviewUrls([existing]);
        return current.map((item) => (
          item.path === path
            ? {
              ...item,
              ...entry,
              path,
              kind: 'pdf',
              language: 'pdf',
              content: '',
              data_url: '',
              pdf_url: pdfUrl,
              pdf_buffer: preview.data,
              preview_size: preview.size,
              loaded_at: Date.now(),
              scroll_hint: options.scrollHint || item.scroll_hint,
              loading: false,
              error: ''
            }
            : item
        ));
      });
    } catch (error) {
      if (
        selectedRef.current?.serverId !== serverId
        || selectedRef.current?.conversationId !== selectedConversationId
      ) {
        return;
      }
      setOpenFiles((current) => current.map((item) => (
        item.path === path ? { ...item, loading: false, error: error?.message || '读取 PDF 失败' } : item
      )));
    }
  }, [selected]);

  const refreshPdfPreview = useCallback((entry, scrollHint) => {
    return loadPdfPreviewIntoTab(entry, { keepExistingPreview: true, scrollHint });
  }, [loadPdfPreviewIntoTab]);

  useEffect(() => {
    if (!selected || !settings) {
      setWorkspaceListings(new Map());
      setWorkspaceExpanded(new Set(['']));
      setWorkspaceError('');
      setOpenFiles((current) => {
        revokeFilePreviewUrls(current);
        return [];
      });
      setActiveFilePath('');
      setConversationLayout(null);
      return undefined;
    }
    const key = selectedConversationUiKey;
    const savedUi = settings.conversationUi?.[key] || {};
    const savedPanels = savedUi.panels || {};
    const savedLayout = layoutSnapshotFromValues({ ...(settings.layout || {}), ...(savedUi.layout || {}) });
    const savedFiles = Array.isArray(savedUi.openFiles)
      ? savedUi.openFiles.map(fileTabSnapshot).filter(Boolean).slice(0, 12)
      : [];
    const savedActivePath = savedFiles.some((file) => file.path === savedUi.activeFilePath)
      ? savedUi.activeFilePath
      : savedFiles[0]?.path || '';
    let disposed = false;
    restoringUiRef.current = true;
    setConversationLayout(savedLayout);
    setOverviewPanelOpen(Boolean(savedPanels.overview));
    setWorkspacePanelOpen(Boolean(savedPanels.workspace));
    setPreviewPanelOpen(Boolean(savedPanels.preview) || savedFiles.length > 0);
    setTerminalOpen(Boolean(savedPanels.terminal));
    setWorkspaceListings(new Map());
    setWorkspaceExpanded(new Set(['']));
    setWorkspaceError('');
    setOpenFiles((current) => {
      revokeFilePreviewUrls(current);
      return savedFiles.map((file) => {
        const savedKind = workspaceFileKind(file.path);
        return {
          ...file,
          kind: savedKind,
          loading: savedKind !== 'presentation'
        };
      });
    });
    setActiveFilePath(savedActivePath);
    queueMicrotask(() => {
      restoringUiRef.current = false;
    });
    const rootCacheParts = [selected.serverId, selected.conversationId, '', 500];
    const cachedRootListing = readWorkspaceResourceCache('workspace-listing', rootCacheParts);
    if (cachedRootListing) {
      setWorkspaceListings((current) => new Map(current).set('', cachedRootListing));
    }
    setWorkspaceLoading((current) => new Set(current).add(''));
    loadWorkspace(selected.serverId, selected.conversationId, '', 500)
      .then((listing) => {
        if (disposed) return;
        writeWorkspaceResourceCache('workspace-listing', rootCacheParts, listing);
        setWorkspaceListings((current) => new Map(current).set('', listing));
      })
      .catch((error) => {
        if (!disposed) setWorkspaceError(error?.message || '读取工作区失败');
      })
      .finally(() => {
        if (disposed) return;
        setWorkspaceLoading((current) => {
          const next = new Set(current);
          next.delete('');
          return next;
        });
    });
    savedFiles.forEach((file) => {
      const savedKind = workspaceFileKind(file.path);
      if (savedKind === 'presentation') {
        return;
      }
      if (savedKind === 'pdf') {
        loadPdfPreviewIntoTab(file);
        return;
      }
      loadWorkspaceFileCached(selected.serverId, selected.conversationId, file.path, undefined, { force: true, cache: false })
        .then((loaded) => {
          if (disposed) return;
          const kind = workspaceFileKind(file.path);
          const data = kind === 'image'
            ? workspaceFileImageDataUrl(file.path, loaded)
            : loaded?.data || '';
          setOpenFiles((current) => current.map((item) => (
            item.path === file.path
              ? {
                ...item,
                ...loaded,
                kind,
                language: fileExtension(file.path),
                content: loaded?.encoding === 'utf8' ? loaded.data || '' : '',
                data_url: kind === 'image' ? data : '',
                loaded_at: Date.now(),
                loading: false
              }
              : item
          )));
        })
        .catch((error) => {
          if (disposed) return;
          setOpenFiles((current) => current.map((item) => (
            item.path === file.path ? { ...item, loading: false, error: error?.message || '读取文件失败' } : item
          )));
        });
    });
    return () => {
      disposed = true;
    };
  }, [selected?.serverId, selected?.conversationId, selectedConversationUiKey, settingsReady, loadPdfPreviewIntoTab, readWorkspaceResourceCache, writeWorkspaceResourceCache, loadWorkspaceFileCached]);

  useEffect(() => {
    if (!selectedConversationUiKey || !settings || restoringUiRef.current) return;
    const files = openFiles.map(fileTabSnapshot).filter(Boolean);
    const snapshot = {
      panels: {
        overview: overviewPanelOpen,
        workspace: workspacePanelOpen,
        preview: previewPanelOpen,
        terminal: terminalOpen
      },
      layout: layoutSnapshotFromValues({
        inspector: overviewPanelWidth,
        file: workspacePanelWidth,
        preview: previewPanelWidth,
        terminal: terminalHeight,
        terminalList: terminalListWidth
      }),
      openFiles: files,
      activeFilePath: files.some((file) => file.path === activeFilePath) ? activeFilePath : ''
    };
    queueConversationUiSave(selectedConversationUiKey, snapshot);
  }, [
    selectedConversationUiKey,
    settingsReady,
    overviewPanelOpen,
    workspacePanelOpen,
    previewPanelOpen,
    terminalOpen,
    overviewPanelWidth,
    workspacePanelWidth,
    previewPanelWidth,
    terminalHeight,
    terminalListWidth,
    openFiles,
    activeFilePath,
    queueConversationUiSave
  ]);

  const toggleWorkspaceDirectory = useCallback((path) => {
    const normalized = normalizeWorkspacePath(path);
    setWorkspaceExpanded((current) => {
      const next = new Set(current);
      if (next.has(normalized)) {
        next.delete(normalized);
      } else {
        next.add(normalized);
        fetchWorkspacePath(normalized).catch(() => {});
      }
      return next;
    });
  }, [fetchWorkspacePath]);

  const openWorkspaceFile = useCallback(async (entry, options = {}) => {
    if (!selected || !entry) return;
    const path = normalizeWorkspacePath(entry.path);
    const conversationId = String(entry.conversationId || entry.conversation_id || selected.conversationId || '').trim();
    if (!conversationId) return;
    setPreviewPanelOpen(true);
    setActiveFilePath(path);
    const entryKind = workspaceEntryKind(entry);
    if (entryKind === 'directory') {
      setOpenFiles((current) => {
        if (current.some((item) => item.path === path)) {
          return current.map((item) => (
            item.path === path && !options.keepExistingPreview
              ? { ...item, loading: true, error: '' }
              : item
          ));
        }
        return [...current, {
          ...entry,
          path,
          name: entry.name || fileNameFromPath(path) || 'workspace',
          kind: 'directory',
          loading: true,
          error: ''
        }];
      });
      try {
        const listing = await loadWorkspace(selected.serverId, conversationId, path, 500);
        setWorkspaceListings((current) => new Map(current).set(path, listing));
        setOpenFiles((current) => current.map((item) => (
          item.path === path
            ? {
              ...item,
              ...entry,
              path,
              name: entry.name || fileNameFromPath(path) || 'workspace',
              kind: 'directory',
              listing,
              entries: Array.isArray(listing?.entries) ? listing.entries : [],
              loaded_at: Date.now(),
              loading: false,
              error: ''
            }
            : item
        )));
      } catch (error) {
        setOpenFiles((current) => current.map((item) => (
          item.path === path ? { ...item, loading: false, error: error?.message || '读取目录失败' } : item
        )));
        if (options.throwOnError) throw error;
      }
      return;
    }
    setOpenFiles((current) => {
      if (current.some((item) => item.path === path)) return current;
      return [...current, { ...entry, path, kind: workspaceFileKind(entry), loading: true }];
    });
    const initialKind = workspaceFileKind(path);
    if (initialKind === 'pdf') {
      await loadPdfPreviewIntoTab({ ...entry, path }, { keepExistingPreview: options.keepExistingPreview });
      return;
    }
    if (initialKind === 'presentation') {
      setOpenFiles((current) => current.map((item) => (
        item.path === path
          ? {
            ...item,
            ...entry,
            path,
            kind: initialKind,
            language: fileExtension(path),
            content: '',
            data: '',
            loading: false
          }
          : item
      )));
      return;
    }
    try {
      const file = await loadWorkspaceFileCached(selected.serverId, conversationId, path, undefined, { force: true, cache: false });
      const kind = workspaceFileKind(path);
      const data = kind === 'image'
        ? workspaceFileImageDataUrl(path, file)
        : file?.data || '';
      setOpenFiles((current) => current.map((item) => (
        item.path === path
          ? {
            ...item,
            ...file,
            kind,
            language: fileExtension(path),
            content: file?.encoding === 'utf8' ? file.data || '' : '',
            data_url: kind === 'image' ? data : '',
            loaded_at: Date.now(),
            loading: false
          }
          : item
      )));
    } catch (error) {
      setOpenFiles((current) => current.map((item) => (
        item.path === path ? { ...item, loading: false, error: error?.message || '读取文件失败' } : item
      )));
      if (options.throwOnError) throw error;
    }
  }, [selected, loadPdfPreviewIntoTab, loadWorkspaceFileCached]);

  const openWorkspacePathTarget = useCallback(async (target) => {
    if (!selected || target?.path === undefined || target?.path === null) return;
    const path = normalizeWorkspacePath(target.path);
    const conversationId = String(target.conversationId || selected.conversationId || '').trim();
    if (!conversationId) return;
    const name = fileNameFromPath(path) || 'workspace';
    const shouldTryDirectory = Boolean(target.explicitDirectory) || !fileExtension(path);
    if (shouldTryDirectory) {
      try {
        await openWorkspaceFile({ path, name, kind: 'directory', conversationId }, { throwOnError: true });
        return;
      } catch (error) {
        if (target.explicitDirectory) throw error;
      }
    }
    await openWorkspaceFile({ path, name, conversationId }, { throwOnError: true });
  }, [selected, openWorkspaceFile]);

  const openChatLocalLink = useCallback((href) => {
    if (!selected) return false;
    const target = workspaceTargetFromLocalLink(href, selected.conversationId, messageAttachmentWorkspaceRoots);
    if (target?.path === undefined || target?.path === null) return false;
    openWorkspacePathTarget(target).catch((error) => {
      window.alert(error?.message || '打开本地链接失败');
    });
    return true;
  }, [selected, messageAttachmentWorkspaceRoots, openWorkspacePathTarget]);

  const refreshWorkspacePreviewFile = useCallback((file) => {
    if (!file || file.path === undefined || file.path === null) return Promise.resolve();
    return openWorkspaceFile(file, { keepExistingPreview: true });
  }, [openWorkspaceFile]);

  const resolveMarkdownAsset = useCallback(async (markdownPath, rawSrc) => {
    const source = String(rawSrc || '').trim();
    if (!source || /^(?:https?:|data:|blob:|file:)/i.test(source)) return source;
    const path = resolveWorkspaceAssetPath(markdownPath, source);
    if (!selected || !path || workspaceFileKind(path) !== 'image') return source;
    const file = await loadWorkspaceFileCached(selected.serverId, selected.conversationId, path, undefined, { force: true, cache: false });
    return workspaceFileImageDataUrl(path, file) || source;
  }, [selected, loadWorkspaceFileCached]);

  const uploadWorkspaceItems = useCallback(async (targetPath, dataTransferItems) => {
    if (!selected || !dataTransferItems?.length) return;
    const id = `upload-${Date.now()}`;
    const target = normalizeWorkspacePath(targetPath);
    try {
      upsertTransfer(id, { type: 'upload', title: '上传工作区文件', detail: '正在读取拖入文件', state: 'running' });
      const files = await collectDroppedFiles(dataTransferItems);
      if (!files.length) {
        finishTransfer(id, { state: 'done', detail: '没有可上传文件' });
        return;
      }
      const stats = uploadPayloadStats(files);
      upsertTransfer(id, { detail: `正在压缩 ${stats.fileCount} 个文件 · ${formatBytes(stats.bytes)}` });
      const archive = await packFilesToTarGz(files);
      if (archive.byteLength > MAX_UPLOAD_COMPRESSED_BYTES) {
        throw new Error(`上传文件过大（压缩后超过 ${formatBytes(MAX_UPLOAD_COMPRESSED_BYTES)}）`);
      }
      upsertTransfer(id, { detail: `正在上传 ${formatBytes(archive.byteLength)}` });
      await window.stellacode2.uploadWorkspace({
        serverId: selected.serverId,
        conversationId: selected.conversationId,
        path: target,
        data: archive
      });
      removeWorkspaceResourceCache('workspace-listing', [selected.serverId, selected.conversationId, target, 500]);
      removeWorkspaceResourceCache('workspace-listing', [selected.serverId, selected.conversationId, parentWorkspacePath(target), 500]);
      removeWorkspaceResourceCache('workspace-file', [selected.serverId, selected.conversationId, target, 'full']);
      removeWorkspaceResourceCache('workspace-file', [selected.serverId, selected.conversationId, target, MESSAGE_IMAGE_PREVIEW_MAX_BYTES]);
      setWorkspaceListings((current) => {
        const next = new Map(current);
        next.delete(target);
        next.delete(parentWorkspacePath(target));
        return next;
      });
      await fetchWorkspacePath(target, { force: true }).catch(() => fetchWorkspacePath(parentWorkspacePath(target), { force: true }));
      finishTransfer(id, { state: 'done', detail: `上传完成 · ${stats.fileCount} 个文件` });
    } catch (error) {
      finishTransfer(id, { state: 'failed', detail: error?.message || '上传失败' });
    }
  }, [selected, upsertTransfer, finishTransfer, fetchWorkspacePath, removeWorkspaceResourceCache]);

  const downloadWorkspaceEntry = useCallback(async (entry) => {
    if (!selected || !entry) return;
    const id = `download-${Date.now()}`;
    const path = normalizeWorkspacePath(entry.path);
    const conversationId = String(entry.conversationId || entry.conversation_id || selected.conversationId || '').trim();
    if (!conversationId) return;
    const kind = workspaceEntryKind(entry) === 'directory' ? 'directory' : 'file';
    try {
      upsertTransfer(id, { type: 'download', title: kind === 'file' ? '下载文件' : '下载文件夹', detail: entry.name || path, state: 'running' });
      const result = await window.stellacode2.downloadWorkspace({
        serverId: selected.serverId,
        conversationId,
        path,
        kind,
        suggestedName: entry.name || fileNameFromPath(path)
      });
      finishTransfer(id, {
        state: result?.saved ? 'done' : 'cancelled',
        detail: result?.saved ? `已保存 ${formatBytes(result.size)}` : '已取消'
      });
    } catch (error) {
      finishTransfer(id, { state: 'failed', detail: error?.message || '下载失败' });
    }
  }, [selected, upsertTransfer, finishTransfer]);

  const openMessageAttachment = useCallback((attachment) => {
    const target = messageAttachmentWorkspaceTarget(attachment);
    const path = target?.path || messageAttachmentWorkspacePath(attachment);
    if (!path || !target?.conversationId) return;
    openWorkspaceFile({
      ...attachment,
      path,
      conversationId: target.conversationId,
      name: attachment.name || fileNameFromPath(path),
      type: attachment.kind
    }).catch(() => {});
  }, [messageAttachmentWorkspacePath, messageAttachmentWorkspaceTarget, openWorkspaceFile]);

  const downloadMessageAttachment = useCallback((attachment) => {
    const target = messageAttachmentWorkspaceTarget(attachment);
    const path = target?.path || messageAttachmentWorkspacePath(attachment);
    if (!path || !target?.conversationId) return;
    downloadWorkspaceEntry({
      ...attachment,
      path,
      conversationId: target.conversationId,
      name: attachment.name || fileNameFromPath(path),
      type: attachment.kind
    }).catch(() => {});
  }, [downloadWorkspaceEntry, messageAttachmentWorkspacePath, messageAttachmentWorkspaceTarget]);

  useEffect(() => {
    if (!selectedServerId || !selectedConversationId) return;
    const serverId = selectedServerId;
    const conversationId = selectedConversationId;
    const sessionId = selectedSessionId;
    const key = conversationKey(serverId, conversationId, sessionId);
    let disposed = false;
    let socketClient = null;

    const cacheMessages = (next) => {
      writeMessageCache(serverId, conversationId, sessionId, next);
    };

    const clearMessageCache = () => {
      removeMessageCache(serverId, conversationId, sessionId);
    };

    const updateSelectedSessionSummary = (latestMessage, latestId, latestIndex) => {
      if (!latestId || !Number.isFinite(latestIndex)) return;
      setConversations((current) => {
        const next = current.map((conversation) => {
          if (conversation.conversation_id !== conversationId) return conversation;
          const session = foregroundSessions(conversation).find((item) => (
            String(item?.id || 'main') === sessionId
          ));
          const currentCount = Number(session?.message_count || conversation?.message_count || 0);
          return patchConversationForegroundSession(conversation, sessionId, {
            last_message_id: String(latestId),
            last_message_time: latestMessage?.message_time || new Date().toISOString(),
            message_count: Math.max(currentCount, latestIndex + 1)
          });
        });
        conversationsRef.current = next;
        return next;
      });
      markConversationRead(serverId, conversationId, sessionId, latestId);
    };

    const applyIncomingMessages = (incoming) => {
      if (!Array.isArray(incoming) || incoming.length === 0 || disposed || websocketKeyRef.current !== key) return;
      const protocolMismatches = committedMessageProtocolMismatches(messagesRef.current, incoming);
      if (protocolMismatches.length > 0) {
        console.warn('stream provisional message differed from durable commit', protocolMismatches);
        setSessionActivity('流式消息和落盘消息不一致，已使用落盘消息');
      }
      const finalizedActivities = streamFinalizedActivityIds(incoming);
      const delta = usageDeltaFromMessages(key, incoming, seenUsageMessagesRef.current);
      if (delta.totalTokens > 0 || delta.cost > 0) {
        setStatusDeltas((current) => {
          const next = new Map(current);
          next.set(key, addUsageTotals(next.get(key), delta));
          return next;
        });
      }
      setMessages((current) => {
        const next = mergeMessages(current, incoming);
        messagesRef.current = next;
        cacheMessages(next);
        return next;
      });
      const latestMessage = incoming.reduce((latest, message) => (
        !latest || messageIndex(message) >= messageIndex(latest) ? message : latest
      ), null);
      const latestId = latestMessage?.id ?? latestMessage?.message_id;
      const latestIndex = latestMessage ? messageIndex(latestMessage) : undefined;
      updateSelectedSessionSummary(latestMessage, latestId, latestIndex);
      const activity = activityFromMessages(incoming);
      if (activity) setSessionActivity(activity);
      if (finalizedActivities.size > 0) {
        updateRunningActivities((current) => current.filter((item) => !finalizedActivities.has(item.id)));
      }
      if (incoming.some((message) => isFinalAssistantMessage(message))) {
        setTimeout(() => {
          if (!disposed && websocketKeyRef.current === key) {
            setRunningActivities([]);
          }
        }, 700);
      }
    };

    const replaceWithRecentMessages = (incoming) => {
      if (!Array.isArray(incoming) || incoming.length === 0 || disposed || websocketKeyRef.current !== key) return;
      messagesRef.current = incoming;
      cacheMessages(incoming);
      setMessages(incoming);
      const latestMessage = incoming.reduce((latest, message) => (
        !latest || messageIndex(message) >= messageIndex(latest) ? message : latest
      ), null);
      const latestId = latestMessage?.id ?? latestMessage?.message_id;
      const latestIndex = latestMessage ? messageIndex(latestMessage) : undefined;
      updateSelectedSessionSummary(latestMessage, latestId, latestIndex);
      const activity = activityFromMessages(incoming);
      if (activity) setSessionActivity(activity);
    };

    const streamBuffers = createStreamBufferStore();
    const streamTracker = createStreamIndexTracker(key);

    const acceptStreamEvent = (event) => {
      return streamTracker.accept(event, (expected, received) => {
        setMessages((current) => {
          const next = applyStreamErrorToMessages(current, {
            ...event,
            error: `non-contiguous stream event: expected index ${expected}, received ${received}`
          });
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('流式消息不连续，已撤销当前临时消息');
      });
    };

    const applySessionStream = (rawEvent) => {
      const event = normalizedStreamEvent(rawEvent);
      const type = streamEventType(event);
      if (!type || disposed || websocketKeyRef.current !== key) return;
      const messageId = streamActivityBaseId(event);
      const scopedChatState = (state) => ({ scopeKey: key, ...state });
      const keepOrSetRunningState = (current, eventPayload) => (
        chatSessionStateIsActive(current) && current.scopeKey === key
          ? current
          : scopedChatState({ state: 'running', currentTurnState: eventPayload })
      );

      if (type === 'turn_started' || type === 'stream_turn_start') {
        streamTracker.reset();
        streamBuffers.reset();
        setChatSessionState(scopedChatState({
          state: 'running',
          currentTurnState: event,
          activeTurnId: String(event?.turn_id || event?.turnId || '').trim()
        }));
        setSessionActivity('正在处理');
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== 'thinking'),
          {
            id: 'thinking',
            title: '正在处理',
            detail: '等待模型响应',
            state: 'running'
          }
        ]);
        return;
      }

      if (type === 'turn_completed' || type === 'stream_turn_done') {
        streamTracker.reset();
        streamBuffers.reset();
        setChatSessionState(scopedChatState({ state: 'idle' }));
        setSessionActivity('已完成');
        const turnId = String(event?.turn_id || event?.turnId || '').trim();
        setMessages((current) => {
          const next = removeStreamingMessagesForTurn(current, turnId);
          if (next !== current) {
            messagesRef.current = next;
            cacheMessages(next);
          }
          return next;
        });
        setTimeout(() => {
          if (!disposed && websocketKeyRef.current === key) {
            setRunningActivities([]);
          }
        }, 700);
        return;
      }

      if (type === 'plan_updated') {
        setChatSessionState((current) => keepOrSetRunningState(current, event));
        const progress = normalizeProgressFeedback({ type: 'turn_progress', progress: event });
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== progress.id && item.id !== 'thinking'),
          mergeProgressActivity(current, progress)
        ]);
        setSessionActivity(progress.detail || progress.title || '已更新计划');
        return;
      }

      if (type === 'stream_assistant_message_delta') {
        if (!acceptStreamEvent(event)) return;
        setChatSessionState((current) => keepOrSetRunningState(current, event));
        const delta = streamDeltaText(event);
        if (!delta) return;
        setMessages((current) => {
          const next = appendStreamAssistantDelta(current, event);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('正在回复');
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== `stream-assistant-${messageId}` && item.id !== 'thinking'),
          mergeProgressActivity(current, {
            id: `stream-assistant-${messageId}`,
            title: '正在回复',
            detail: shortText(delta, 72),
            state: 'running'
          })
        ]);
        return;
      }

      if (type === 'stream_reasoning_summary_part_added') {
        if (!acceptStreamEvent(event)) return;
        setChatSessionState((current) => keepOrSetRunningState(current, event));
        setSessionActivity('思考中');
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== `stream-reasoning-${messageId}` && item.id !== 'thinking'),
          mergeProgressActivity(current, {
            id: `stream-reasoning-${messageId}`,
            title: '思考中',
            detail: '整理推理摘要',
            state: 'running'
          })
        ]);
        return;
      }

      if (type === 'stream_reasoning_summary_delta') {
        if (!acceptStreamEvent(event)) return;
        setChatSessionState((current) => keepOrSetRunningState(current, event));
        const summaryIndex = event?.summary_index ?? event?.summaryIndex ?? 0;
        const bufferKey = `${key}:reasoning:${messageId}:${summaryIndex}`;
        const text = streamBuffers.append(bufferKey, streamDeltaText(event));
        setMessages((current) => {
          const next = appendStreamReasoningSummary(current, event);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('思考中');
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== `stream-reasoning-${messageId}` && item.id !== 'thinking'),
          mergeProgressActivity(current, {
            id: `stream-reasoning-${messageId}`,
            title: '思考中',
            detail: shortText(text || '整理推理摘要', 96),
            state: 'running'
          })
        ]);
        return;
      }

      if (type === 'stream_tool_call_delta') {
        if (!acceptStreamEvent(event)) return;
        setChatSessionState((current) => keepOrSetRunningState(current, event));
        const itemId = streamItemId(event);
        const bufferKey = `${key}:tool:${itemId}`;
        const text = streamBuffers.append(bufferKey, streamDeltaText(event));
        setMessages((current) => {
          const next = appendStreamToolCallDelta(current, event);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('准备调用工具');
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== `stream-tool-${itemId}` && item.id !== 'thinking'),
          mergeProgressActivity(current, {
            id: `stream-tool-${itemId}`,
            title: '准备调用工具',
            detail: shortText(text, 96),
            state: 'running'
          })
        ]);
        return;
      }

      if (type === 'stream_tool_result_done') {
        if (!acceptStreamEvent(event)) return;
        setChatSessionState((current) => keepOrSetRunningState(current, event));
        const toolResult = event?.tool_result || event?.toolResult || {};
        const itemId = String(toolResult.tool_call_id || toolResult.toolCallId || event?.batch_id || event?.batchId || streamItemId(event)).trim();
        const toolName = String(toolResult.tool_name || toolResult.toolName || '工具').trim();
        setMessages((current) => {
          const next = appendStreamToolResultDone(current, event);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity(`${toolName} 已返回`);
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== `stream-tool-${itemId}` && item.id !== `stream-tool-result-${itemId}` && item.id !== 'thinking'),
          mergeProgressActivity(current, {
            id: `stream-tool-result-${itemId || toolName}`,
            title: `${toolName} 已返回`,
            detail: toolName,
            state: 'running'
          })
        ]);
        return;
      }

      if (type === 'stream_error') {
        streamTracker.clearForEvent(event);
        setChatSessionState(scopedChatState({ state: 'failed', lastError: streamErrorText(event) }));
        const error = streamErrorText(event);
        setMessages((current) => {
          const next = applyStreamErrorToMessages(current, event);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity(error);
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== `stream-assistant-${messageId}` && item.id !== `stream-reasoning-${messageId}` && item.id !== 'thinking'),
          mergeProgressActivity(current, {
            id: `stream-error-${messageId}`,
            title: '响应失败',
            detail: shortText(error, 96),
            state: 'failed'
          })
        ]);
      }
    };

    const loadInitialMessagePage = async () => {
      const conversation = conversationsRef.current.find((item) => item.conversation_id === conversationId);
      const session = foregroundSessions(conversation).find((item) => String(item?.id || 'main') === sessionId) || conversation;
      const initial = await loadMessages(
        serverId,
        conversationId,
        { ...recentMessagePageParams(session), foregroundSessionId: sessionId }
      );
      if (disposed || websocketKeyRef.current !== key) return;
      setMessages((current) => {
        const next = current.length ? mergeMessages(current, initial) : initial;
        messagesRef.current = next;
        cacheMessages(next);
        return next;
      });
      setMessagesReady(true);
    };

    const reconcileAck = async (ack) => {
      const total = Number(ack?.next_message_index ?? ack?.total ?? ack?.next_message_id);
      if (!Number.isFinite(total) || disposed || websocketKeyRef.current !== key) return;
      const current = messagesRef.current;
      const lastIndex = lastServerMessageIndex(current);
      if (lastIndex === undefined) {
        if (total <= 0) {
          messagesRef.current = [];
          clearMessageCache();
          setMessages([]);
          setMessagesReady(true);
          return;
        }
        const initial = await loadMessages(
          serverId,
          conversationId,
          { ...recentMessagePageParams(null, 40, total), foregroundSessionId: sessionId }
        );
        if (!disposed && websocketKeyRef.current === key) {
          setMessages((current) => {
            const next = current.length ? mergeMessages(current, initial) : initial;
            messagesRef.current = next;
            cacheMessages(next);
            return next;
          });
          setMessagesReady(true);
        }
        return;
      }
      if (total > lastIndex + 1) {
        const gap = total - lastIndex - 1;
        const shouldJumpToTail = gap > 200;
        const params = shouldJumpToTail
          ? recentMessagePageParams(null, 80, total)
          : { offset: lastIndex + 1, limit: gap };
        const missing = await loadMessages(serverId, conversationId, {
          ...params,
          foregroundSessionId: sessionId
        });
        if (shouldJumpToTail) {
          replaceWithRecentMessages(missing);
        } else {
          applyIncomingMessages(missing);
        }
        if (!disposed && websocketKeyRef.current === key) {
          setMessagesReady(true);
        }
      }
    };

    const applyChatSnapshotLiveProjection = (snapshot) => {
      if (!snapshot || disposed || websocketKeyRef.current !== key) return;
      const snapshotState = chatSnapshotState(snapshot);
      const provisional = snapshot.current_provisional_assistant_message?.message;
      if (provisional) {
        const turnId = String(snapshot.current_turn_state?.turn_id || snapshot.current_turn_state?.turnId || '').trim();
        const projected = {
          ...provisional,
          id: provisional.id || provisional.message_id || provisional.messageId || turnId || undefined,
          message_id: provisional.message_id || provisional.messageId || provisional.id || turnId || undefined,
          _streamTurnId: turnId || provisional._streamTurnId || '',
          _streaming: true
        };
        setMessages((current) => {
          const next = mergeMessages(current, [projected]);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('正在回复');
      }
      const toolStates = Array.isArray(snapshot.running_tool_results)
        ? snapshot.running_tool_results
        : [];
      const queuedMessages = Array.isArray(snapshot.queued_outbound_messages)
        ? snapshot.queued_outbound_messages
        : [];
      if (queuedMessages.length > 0) {
        setMessages((current) => {
          let next = current;
          queuedMessages.forEach((queued) => {
            next = markQueuedUserMessage(next, queued?.client_message_id || queued?.clientMessageId);
          });
          messagesRef.current = next;
          return next;
        });
      }
      const activities = toolStates
        .filter((state) => !state?.committed)
        .map((state) => state?.tool_result || state?.toolResult || state)
        .filter(Boolean)
        .map((toolResult) => {
          const itemId = String(toolResult.tool_call_id || toolResult.toolCallId || toolResult.tool_name || toolResult.toolName || '').trim();
          const toolName = String(toolResult.tool_name || toolResult.toolName || '工具').trim();
          return {
            id: `stream-tool-result-${itemId || toolName}`,
            title: `${toolName} 已返回`,
            detail: toolName,
            state: 'running'
          };
        });
      if (activities.length > 0) {
        setMessages((current) => {
          let next = current;
          toolStates.forEach((state) => {
            if (state?.committed) return;
            next = appendStreamToolResultDone(next, {
              turn_id: state?.turn_id || state?.turnId || snapshot.current_turn_state?.turn_id,
              tool_result: state?.tool_result || state?.toolResult || state
            });
          });
          messagesRef.current = next;
          return next;
        });
        updateRunningActivities((current) => [
          ...current.filter((item) => !activities.some((activity) => activity.id === item.id) && item.id !== 'thinking'),
          ...activities
        ]);
        setSessionActivity(activities[activities.length - 1]?.title || '正在处理');
      } else if (snapshotState.state === 'running' && !provisional) {
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== 'thinking'),
          {
            id: 'thinking',
            title: '正在处理',
            detail: '等待模型响应',
            state: 'running'
          }
        ]);
        setSessionActivity('正在处理');
      } else if (snapshotState.state === 'idle') {
        setMessages((current) => {
          const next = removeStreamingMessagesForTurn(current);
          if (next !== current) {
            messagesRef.current = next;
            cacheMessages(next);
          }
          return next;
        });
        setRunningActivities([]);
      }
    };

    const applyChatSocketPayload = (payload) => {
      if (disposed || websocketKeyRef.current !== key) return;
      const payloadType = String(payload?.type || '');
      if (payloadType === 'chat.snapshot') {
        const snapshotState = chatSnapshotState(payload);
        setChatSessionState({ scopeKey: key, ...snapshotState });
        setSessionActivity(payload.reason === 'session_changed' ? 'Session 已切换' : '实时连接已同步');
        reconcileAck(payload).catch(() => {});
        applyChatSnapshotLiveProjection(payload);
      } else if (payloadType === 'chat.user_message_queued') {
        setChatSessionState({ scopeKey: key, state: 'queued' });
        setMessages((current) => {
          const next = markQueuedUserMessage(current, payload.client_message_id || payload.clientMessageId);
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('消息已排队');
      } else if (payloadType === 'chat.user_message_started') {
        setChatSessionState((current) => chatSessionStateIsActive(current) && current.scopeKey === key ? current : { scopeKey: key, state: 'queued' });
        setSessionActivity('开始处理');
      } else if (payloadType === 'chat.user_message_committed') {
        setChatSessionState((current) => chatSessionStateIsActive(current) && current.scopeKey === key ? current : { scopeKey: key, state: 'queued' });
        applyIncomingMessages(payload.message ? [payload.message] : []);
        setSessionActivity('用户消息已落盘');
      } else if (payloadType === 'chat.message_appended') {
        applyIncomingMessages(payload.message ? [payload.message] : []);
      } else if (
        payloadType.startsWith('chat.stream_')
        || payloadType === 'chat.plan_updated'
      ) {
        applySessionStream(payload);
      } else if (payloadType === 'error') {
        setChatSessionState({ scopeKey: key, state: 'failed', lastError: payload.message || payload.error || '实时连接错误' });
        setSessionActivity(payload.message || payload.error || '实时连接错误');
      }
    };

    const loadFallbackMessagePage = () => {
      const conversation = conversationsRef.current.find((item) => item.conversation_id === conversationId);
      const session = foregroundSessions(conversation).find((item) => String(item?.id || 'main') === sessionId) || conversation;
      loadMessages(serverId, conversationId, {
        ...recentMessagePageParams(session),
        foregroundSessionId: sessionId
      })
        .then((initial) => {
          if (disposed || websocketKeyRef.current !== key) return;
          setMessages((current) => {
            const next = current.length ? mergeMessages(current, initial) : initial;
            messagesRef.current = next;
            cacheMessages(next);
            return next;
          });
          setMessagesReady(true);
        })
        .catch(() => {
          if (!disposed) {
            setMessages([]);
            setMessagesReady(true);
          }
        });
    };

    socketClient?.close();
    websocketKeyRef.current = key;
    const cachedMessages = readMessageCache(serverId, conversationId, sessionId);
    messagesRef.current = cachedMessages;
    setMessages(cachedMessages);
    setMessagesReady(cachedMessages.length > 0);
    setSessionActivity('');
    setChatSessionState({ scopeKey: key, state: 'idle' });
    setRunningActivities([]);
    loadInitialMessagePage().catch(() => {
      if (!disposed && websocketKeyRef.current === key && messagesRef.current.length === 0) {
        setMessages([]);
        setMessagesReady(true);
      }
    });
    socketClient = startChatSocketClient({
      serverId,
      conversationId,
      foregroundSessionId: sessionId,
      isCurrent: () => !disposed && websocketKeyRef.current === key,
      onPayload: applyChatSocketPayload,
      onStatus: (status) => {
        if (status === 'reconnecting') setSessionActivity('实时连接异常，正在重连');
        else if (status === 'error') setSessionActivity('实时连接异常');
        else if (status === 'unavailable') setSessionActivity('实时连接不可用，使用刷新兜底');
      },
      onFallback: loadFallbackMessagePage
    });

    return () => {
      disposed = true;
      if (websocketKeyRef.current === key) websocketKeyRef.current = '';
      socketClient?.close();
    };
  }, [selectedServerId, selectedConversationId, selectedSessionId, markConversationRead]);

  const toggleSidebar = () => {
    const nextMode = sidebarMode === 'collapsed' ? 'expanded' : 'collapsed';
    setSidebarMode(nextMode);
    if (settings) {
      saveSettings({ ...settings, sidebarMode: nextMode }).catch(() => {});
    }
  };

  const resizeLayout = (kind, event) => {
    if (!settings) return;
    event.preventDefault();
    const handle = event.currentTarget;
    try {
      handle.setPointerCapture?.(event.pointerId);
    } catch {}
    const root = event.currentTarget.closest('.app-root');
    root?.classList.add('layout-resizing');
    const scroll = root?.querySelector('.message-scroll');
    const bottomOffset = scroll ? scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight : 0;
    const startX = event.clientX;
    const startY = event.clientY;
    const terminalListMin = kind === 'terminalList'
      ? measuredTerminalListMin(root)
      : TERMINAL_LIST_MIN;
    const startLayout = {
      ...(settings.layout || {}),
      sidebar: sidebarWidth,
      inspector: overviewPanelWidth,
      file: workspacePanelWidth,
      preview: previewPanelWidth,
      terminal: terminalHeight,
      terminalList: terminalListWidth
    };
    let latestLayout = startLayout;
    layoutDraftRef.current = startLayout;
    let raf = 0;
    const applyLayoutVars = () => {
      if (!root) return;
      const sidebar = sidebarMode === 'collapsed' ? SIDEBAR_COLLAPSED : latestLayout.sidebar;
      const previewRight = workspacePanelOpen ? latestLayout.file : 0;
      const overviewRight = previewRight + (previewPanelOpen ? latestLayout.preview : 0);
      const contentRight = (overviewPanelOpen ? latestLayout.inspector : 0)
        + (workspacePanelOpen ? latestLayout.file : 0)
        + (previewPanelOpen ? latestLayout.preview : 0);
      root.style.setProperty('--sidebar-width', `${sidebar}px`);
      root.style.setProperty('--overview-panel-width', `${latestLayout.inspector}px`);
      root.style.setProperty('--overview-panel-right', `${overviewRight}px`);
      root.style.setProperty('--workspace-panel-width', `${latestLayout.file}px`);
      root.style.setProperty('--preview-panel-width', `${latestLayout.preview}px`);
      root.style.setProperty('--preview-panel-right', `${previewRight}px`);
      root.style.setProperty('--terminal-height-live', `${latestLayout.terminal}px`);
      root.style.setProperty('--terminal-list-width-live', `${latestLayout.terminalList}px`);
      root.style.setProperty('--content-right', `${contentRight}px`);
      if (scroll) {
        scroll.scrollTop = scroll.scrollHeight - scroll.clientHeight - bottomOffset;
      }
    };
    const move = (moveEvent) => {
      const delta = moveEvent.clientX - startX;
      if (kind === 'sidebar' && sidebarMode !== 'collapsed') {
        latestLayout = {
          ...latestLayout,
          sidebar: clamp(startLayout.sidebar + delta, 220, 520)
        };
      } else if (kind === 'terminal' && terminalOpen) {
        const deltaY = moveEvent.clientY - startY;
        latestLayout = {
          ...latestLayout,
          terminal: clamp(startLayout.terminal - deltaY, TERMINAL_HEIGHT_MIN, TERMINAL_HEIGHT_MAX)
        };
      } else if (kind === 'terminalList' && terminalOpen) {
        latestLayout = {
          ...latestLayout,
          terminalList: clamp(startLayout.terminalList + delta, terminalListMin, TERMINAL_LIST_MAX)
        };
      } else if (kind === 'workspace' && workspacePanelOpen) {
        latestLayout = {
          ...latestLayout,
          file: clamp(startLayout.file - delta, WORKSPACE_PANEL_MIN, WORKSPACE_PANEL_MAX)
        };
      } else if (kind === 'preview' && previewPanelOpen) {
        latestLayout = {
          ...latestLayout,
          preview: clamp(startLayout.preview - delta, 320, 820)
        };
      } else if (kind === 'overview' && overviewPanelOpen) {
        latestLayout = {
          ...latestLayout,
          inspector: clamp(startLayout.inspector - delta, 320, 760)
        };
      }
      if (!raf) {
        raf = window.requestAnimationFrame(() => {
          raf = 0;
          layoutDraftRef.current = latestLayout;
          applyLayoutVars();
        });
      }
    };
    const finish = () => {
      if (raf) {
        window.cancelAnimationFrame(raf);
        raf = 0;
      }
      applyLayoutVars();
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', finish);
      window.removeEventListener('pointercancel', finish);
      try {
        handle.releasePointerCapture?.(event.pointerId);
      } catch {}
      if (selectedKey && kind !== 'sidebar') {
        setConversationLayout(latestLayout);
      } else {
        setSettings((prev) => prev ? { ...prev, layout: { ...(prev.layout || {}), ...latestLayout } } : prev);
      }
      const finalPreviewRight = workspacePanelOpen ? latestLayout.file : 0;
      const finalOverviewRight = finalPreviewRight + (previewPanelOpen ? latestLayout.preview : 0);
      const finalContentRight = (overviewPanelOpen ? latestLayout.inspector : 0)
        + (workspacePanelOpen ? latestLayout.file : 0)
        + (previewPanelOpen ? latestLayout.preview : 0);
      root?.style.setProperty('--preview-panel-right', `${finalPreviewRight}px`);
      root?.style.setProperty('--overview-panel-right', `${finalOverviewRight}px`);
      root?.style.setProperty('--content-right', `${finalContentRight}px`);
      layoutDraftRef.current = null;
      window.requestAnimationFrame(() => {
        root?.style.removeProperty('--terminal-height-live');
        root?.style.removeProperty('--terminal-list-width-live');
        root?.classList.remove('layout-resizing');
      });
      if (!selectedKey || kind === 'sidebar') {
        saveSettings({ ...settings, layout: { ...(settings.layout || {}), ...latestLayout } }).catch(() => {});
      }
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', finish);
    window.addEventListener('pointercancel', finish);
  };

  const loadOlderMessages = useCallback(async () => {
    if (!selected || loadingOlderRef.current || !hasOlderMessages(messagesRef.current)) return false;
    const key = conversationKey(selected.serverId, selected.conversationId, selectedSessionId);
    const anchorId = firstMessageId(messagesRef.current);
    if (!anchorId) return false;
    loadingOlderRef.current = true;
    try {
      const anchor = messagesRef.current[0];
      const anchorIndex = Math.max(0, messageIndex(anchor) || 0);
      const offset = Math.max(0, anchorIndex - 40);
      const limit = anchorIndex - offset;
      const older = limit > 0 ? await loadMessages(selected.serverId, selected.conversationId, {
        offset,
        limit,
        foregroundSessionId: selectedSessionId
      }) : [];
      if (websocketKeyRef.current !== key || !older.length) return false;
      setMessages((current) => {
        const next = mergeMessages(current, older);
        messagesRef.current = next;
        writeMessageCache(selected.serverId, selected.conversationId, selectedSessionId, next);
        return next;
      });
      return true;
    } finally {
      loadingOlderRef.current = false;
    }
  }, [selected, selectedSessionId]);

  const loadAvailableModels = useCallback(async () => {
    if (!selected?.serverId) return [];
    return loadModels(selected.serverId);
  }, [selected?.serverId]);

  const loadPropertiesModels = useCallback(async () => {
    if (!activeServerId) return [];
    setPropertiesModelsLoading(true);
    setPropertiesModelsError('');
    try {
      const nextModels = await loadModels(activeServerId);
      setPropertiesModels(Array.isArray(nextModels) ? nextModels : []);
      return nextModels;
    } catch (error) {
      setPropertiesModels([]);
      setPropertiesModelsError(error?.message || '无法读取模型列表');
      return [];
    } finally {
      setPropertiesModelsLoading(false);
    }
  }, [activeServerId]);

  const postConversationControlCommand = useCallback(async (conversation, command) => {
    if (!activeServerId || !conversation || !command) return;
    const targetSessionId = selected?.conversationId === conversation.conversation_id
      ? selectedSessionId
      : foregroundSessions(conversation).find((session) => session?.is_main)?.id || foregroundSessions(conversation)[0]?.id || 'main';
    setPropertiesApplying(true);
    try {
      await postConversationMessage(activeServerId, conversation.conversation_id, command, activeUserName, [], [], targetSessionId);
      const list = await loadConversations(activeServerId);
      setConversations(list);
      if (selected?.conversationId === conversation.conversation_id) {
        setSessionActivity(controlCommandActivity(command));
      }
    } catch (error) {
      window.alert(error?.message || '更新 Conversation 设置失败');
    } finally {
      setPropertiesApplying(false);
    }
  }, [activeServerId, activeUserName, selected?.conversationId, selectedSessionId]);

  const switchConversationModel = useCallback((conversation, model) => {
    const alias = String(model || '').trim();
    if (!alias) return;
    postConversationControlCommand(conversation, `/model ${alias}`);
  }, [postConversationControlCommand]);

  const switchConversationReasoning = useCallback((conversation, effort) => {
    const value = String(effort || '').trim();
    if (!value) return;
    postConversationControlCommand(conversation, `/reasoning ${value}`);
  }, [postConversationControlCommand]);

  const switchConversationIdleCompact = useCallback((conversation, value) => {
    const normalized = String(value || '').trim();
    if (!normalized) return;
    postConversationControlCommand(conversation, `/idle_compact ${normalized}`);
  }, [postConversationControlCommand]);

  const sendMessage = useCallback(async (text, files = [], selections = []) => {
    const value = String(text || '').trim();
    const outgoingFiles = Array.isArray(files) ? files : [];
    const outgoingSelections = Array.isArray(selections) ? selections : [];
    if ((!value && outgoingFiles.length === 0 && outgoingSelections.length === 0) || !selected || sending) return false;
    const key = conversationKey(selected.serverId, selected.conversationId, selectedSessionId);
    const commandState = outgoingFiles.length > 0
      ? { control: false, name: '', title: '等待响应', detail: '消息已送达，等待模型开始处理' }
      : slashCommandState(value);
    const previousLastServerIndex = lastServerMessageIndex(messagesRef.current);
    const optimistic = {
      id: `local-${Date.now()}`,
      role: 'user',
      user_name: activeUserName,
      text: value,
      preview: value,
      files: outgoingFiles,
      items: outgoingSelections.map((selection, index) => ({
        type: 'selection_reference',
        index,
        selection
      })),
      attachment_count: outgoingFiles.length,
      message_time: new Date().toISOString(),
      _optimistic: true,
      pending: true
    };
    setSending(true);
    setSessionActivity('正在发送');
    updateRunningActivities(() => [
      { id: 'sending', title: '发送中', detail: shortText(value || (outgoingSelections.length ? `${outgoingSelections.length} 个选区引用` : `${outgoingFiles.length} 个附件`)), state: 'running' }
    ]);
    setMessages((current) => {
      const next = [...current, optimistic];
      messagesRef.current = next;
      return next;
    });
    try {
      await postConversationMessage(selected.serverId, selected.conversationId, value, activeUserName, outgoingFiles, outgoingSelections, selectedSessionId, optimistic.id);
      setSelectionReferences((current) => current.filter((item) => !outgoingSelections.some((sent) => sent.id === item.id)));
      if (websocketKeyRef.current !== key) return false;
      setMessages((current) => {
        const next = commandState.control
          ? current.filter((message) => message.id !== optimistic.id)
          : current.map((message) => (
              message.id === optimistic.id ? { ...message, pending: false } : message
            ));
        messagesRef.current = next;
        return next;
      });
      if (commandState.control) {
        setSessionActivity(commandState.detail);
        updateRunningActivities(() => [
          { id: 'command-sent', title: commandState.title, detail: commandState.detail, state: 'done' }
        ]);
        window.setTimeout(() => {
          if (websocketKeyRef.current === key) setRunningActivities([]);
        }, 900);
      } else {
        setSessionActivity('已发送，等待响应');
        updateRunningActivities((current) => [
          ...current.filter((item) => item.id !== 'sending'),
          { id: 'waiting-response', title: commandState.title, detail: commandState.detail, state: 'running' }
        ]);
      }
      if (!commandState.control) {
        const offset = previousLastServerIndex !== undefined ? previousLastServerIndex + 1 : 0;
        const incoming = await loadMessages(selected.serverId, selected.conversationId, {
          offset,
          limit: 80,
          foregroundSessionId: selectedSessionId
        });
        if (websocketKeyRef.current === key) {
          setMessages((current) => {
            const next = mergeMessages(current, incoming);
            messagesRef.current = next;
            writeMessageCache(selected.serverId, selected.conversationId, selectedSessionId, next);
            return next;
          });
        }
      }
      return true;
    } catch (error) {
      if (websocketKeyRef.current === key) {
        setSessionActivity(error?.message || '发送失败');
        updateRunningActivities(() => [
          { id: 'send-error', title: '发送失败', detail: error?.message || '发送失败', state: 'failed' }
        ]);
        setMessages((current) => {
          const next = current.map((message) => (
            message.id === optimistic.id ? { ...message, pending: false, error: error?.message || '发送失败' } : message
          ));
          messagesRef.current = next;
          return next;
        });
      }
      return false;
    } finally {
      if (websocketKeyRef.current === key) setSending(false);
    }
  }, [selected, selectedSessionId, sending, activeUserName]);

  const addSelectionReference = useCallback((selection) => {
    if (!selection?.file_path || !selection?.selected_text) return;
    const id = selection.id || `${selection.file_path}-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    setSelectionReferences((current) => [
      ...current.filter((item) => item.id !== id),
      { ...selection, id }
    ].slice(-8));
  }, []);

  const title = activeConversation
    ? displayForegroundSessionName(activeForegroundSession, activeConversation)
    : 'Stellacode';
  const subtitle = activeConversation
    ? [displayConversationName(activeConversation), formatModel(activeConversation, selectedConversationStatus), sessionActivity].filter(Boolean).join(' · ')
    : '选择或创建一个 Conversation';
  const conversationListUi = settings?.conversationListUi?.[activeServerId] || {};

  return (
    <div
      className={`app-root sidebar-${sidebarMode}${rightContentInset ? ' right-panel-open' : ''}${workspacePanelOpen ? ' workspace-panel-open' : ''}${previewPanelOpen ? ' preview-panel-open' : ''}${terminalOpen ? ' terminal-open' : ''}`}
      data-theme={settings?.themeMode || 'system'}
      style={{
        ...themeVariables,
        '--sidebar-width': `${sidebarWidth}px`,
        '--overview-panel-width': `${overviewPanelWidth}px`,
        '--overview-panel-right': `${overviewPanelRight}px`,
        '--workspace-panel-width': `${workspacePanelWidth}px`,
        '--preview-panel-width': `${previewPanelWidth}px`,
        '--preview-panel-right': `${previewPanelRight}px`,
        '--terminal-height': `${terminalHeight}px`,
        '--terminal-list-width': `${terminalListWidth}px`,
        '--app-font-size': `${displayFontSize}px`,
        '--ui-scale': uiScale,
        '--content-right': `${rightContentInset}px`
      }}
    >
      <div className="sidebar-chrome-fill" aria-hidden="true" />
      <WindowChrome
        title={title}
        subtitle={subtitle}
        transfers={transfers}
        sidebarWidth={sidebarWidth}
        sidebarMode={sidebarMode}
        onToggleSidebar={toggleSidebar}
        onNewConversation={() => setNewConversationOpen(true)}
        overviewPanelOpen={overviewPanelOpen}
        workspacePanelOpen={workspacePanelOpen}
        previewPanelOpen={previewPanelOpen}
        updateReady={updateReady}
        onToggleOverview={() => setOverviewPanelOpen((value) => !value)}
        onToggleWorkspace={() => setWorkspacePanelOpen((value) => !value)}
        onTogglePreview={() => setPreviewPanelOpen((value) => !value)}
        onToggleTerminal={() => setTerminalOpen((value) => !value)}
        onInstallUpdate={() => window.stellacode2?.updater?.install?.()}
      />
      <ConversationBar
        serverId={activeServerId}
        sidebarMode={sidebarMode}
        conversations={conversations}
        hiddenConversationIds={settings?.hiddenConversations?.[activeServerId] || []}
        conversationOrder={conversationListUi.order || []}
        openConversationIds={conversationListUi.openConversationIds || []}
        statuses={statuses}
        selected={selected}
        loading={loading}
        activeRunningKey={runningActivities.length > 0 && chatSessionStateIsActive(chatSessionState) ? chatSessionState.scopeKey || '' : ''}
        onSelect={setSelected}
        onOpenSettings={() => setSettingsOpen(true)}
        onRename={openRenameConversationDialog}
        onOpenProperties={(conversation) => setPropertiesConversation(conversation)}
        onHide={(conversation) => setConversationHidden(conversation, true)}
        onUnhide={(conversation) => setConversationHidden(conversation, false)}
        onDelete={deleteSelectedConversation}
        onConversationOrderChange={(order) => updateConversationListUi({ order })}
        onOpenFoldersChange={(openConversationIds) => updateConversationListUi({ openConversationIds })}
        onCreateSession={createConversationForegroundSession}
        onRenameSession={renameConversationForegroundSession}
        onDeleteSession={deleteConversationForegroundSession}
      />
      {sidebarMode !== 'collapsed' && (
        <button
          className="layout-handle sidebar-handle"
          type="button"
          aria-label="调整 Conversation Bar 宽度"
          onPointerDown={(event) => resizeLayout('sidebar', event)}
        />
      )}
      <main className="content-area">
        <ChatWorkspace
          title={title}
          conversationKey={selectedKey}
          modelSelectionPending={Boolean(activeConversation?.model_selection_pending ?? selectedConversationStatus?.model_selection_pending)}
          messages={messages}
          messagesReady={messagesReady}
          mode={composerMode}
          hasOlder={hasOlderMessages(messages)}
          onLoadOlder={loadOlderMessages}
          onSend={sendMessage}
          onLoadModels={loadAvailableModels}
          sending={sending}
          processing={selectedProcessing}
          runningActivities={runningActivities}
          selectionReferences={selectionReferences}
          onRemoveSelectionReference={(id) => setSelectionReferences((current) => current.filter((item) => item.id !== id))}
          onOpenAttachment={openMessageAttachment}
          onDownloadAttachment={downloadMessageAttachment}
          onResolveAttachmentUrl={resolveMessageAttachmentUrl}
          onOpenLocalLink={openChatLocalLink}
        />
      </main>
      <OverviewPanel
        open={overviewPanelOpen}
        conversation={activeConversation}
        status={selectedConversationStatus}
        usage={selectedUsage}
        title={title}
      />
      <WorkspacePanel
        open={workspacePanelOpen}
        selected={selected}
        listings={workspaceListings}
        expanded={workspaceExpanded}
        loading={workspaceLoading}
        error={workspaceError}
        status={selectedConversationStatus}
        activeFilePath={activeFilePath}
        onRefresh={() => fetchWorkspacePath('', { force: true }).catch(() => {})}
        onToggleDirectory={toggleWorkspaceDirectory}
        onOpenFile={openWorkspaceFile}
        onUpload={uploadWorkspaceItems}
        onDownload={downloadWorkspaceEntry}
      />
      <FilePreviewPanel
        open={previewPanelOpen}
        openFiles={openFiles}
        activeFilePath={activeFilePath}
        onSelectFile={setActiveFilePath}
        onDownloadFile={downloadWorkspaceEntry}
        onRefreshFile={refreshWorkspacePreviewFile}
        onRefreshPdfPreview={refreshPdfPreview}
        onResolveMarkdownAsset={resolveMarkdownAsset}
        onCreateSelectionReference={addSelectionReference}
        onOpenFile={openWorkspaceFile}
        onCloseFile={(path) => {
          setOpenFiles((items) => {
            revokeFilePreviewUrls(items.filter((item) => item.path === path));
            const next = items.filter((item) => item.path !== path);
            if (activeFilePath === path) {
              setActiveFilePath(next.at(-1)?.path || '');
            }
            if (next.length === 0) {
              setPreviewPanelOpen(false);
            }
            return next;
          });
        }}
      />
      {workspacePanelOpen && (
        <button
          className="layout-handle workspace-panel-handle"
          type="button"
          aria-label="调整工作区文件面板宽度"
          onPointerDown={(event) => resizeLayout('workspace', event)}
        />
      )}
      {overviewPanelOpen && (
        <button
          className="layout-handle overview-panel-handle"
          type="button"
          aria-label="调整 Conversation 概览面板宽度"
          onPointerDown={(event) => resizeLayout('overview', event)}
        />
      )}
      {previewPanelOpen && (
        <button
          className="layout-handle preview-panel-handle"
          type="button"
          aria-label="调整文件预览面板宽度"
          onPointerDown={(event) => resizeLayout('preview', event)}
        />
      )}
      <TerminalDock
        open={terminalOpen}
        serverId={selected?.serverId || ''}
        conversationId={selected?.conversationId || ''}
        fontSize={terminalFontSize}
        onResizeHeight={(event) => resizeLayout('terminal', event)}
        onResizeList={(event) => resizeLayout('terminalList', event)}
      />
      <NewConversationDialog
        open={newConversationOpen}
        servers={settings?.servers || []}
        activeServerId={activeServerId}
        creating={creatingConversation}
        onOpenChange={setNewConversationOpen}
        onCreate={createNewConversation}
      />
      <RenameConversationDialog
        open={Boolean(renamingConversation)}
        conversation={renamingConversation}
        saving={renamingConversationSaving}
        onOpenChange={(open) => {
          if (!open && !renamingConversationSaving) setRenamingConversation(null);
        }}
        onRename={renameSelectedConversation}
      />
      <RenameSessionDialog
        open={Boolean(renamingSession)}
        conversation={renamingSession?.conversation || null}
        session={renamingSession?.session || null}
        saving={renamingSessionSaving}
        onOpenChange={(open) => {
          if (!open && !renamingSessionSaving) setRenamingSession(null);
        }}
        onRename={renameSelectedForegroundSession}
      />
      <ConversationPropertiesDialog
        open={Boolean(propertiesConversationCurrent)}
        conversation={propertiesConversationCurrent}
        status={propertiesConversationCurrent ? statuses.get(conversationKey(activeServerId, propertiesConversationCurrent.conversation_id, 'main')) : null}
        models={propertiesModels}
        modelsLoading={propertiesModelsLoading}
        modelsError={propertiesModelsError}
        applying={propertiesApplying}
        onOpenChange={(open) => {
          if (!open && !propertiesApplying) setPropertiesConversation(null);
        }}
        onLoadModels={loadPropertiesModels}
        onSwitchModel={switchConversationModel}
        onSwitchReasoning={switchConversationReasoning}
        onSwitchIdleCompact={switchConversationIdleCompact}
      />
      <SettingsDialog
        open={settingsOpen}
        settings={settings}
        saving={settingsSaving}
        onOpenChange={setSettingsOpen}
        onSave={saveSettingsFromDialog}
      />
    </div>
  );
}

createRoot(document.getElementById('root')).render(
  <AppErrorBoundary>
    <App />
  </AppErrorBoundary>
);
