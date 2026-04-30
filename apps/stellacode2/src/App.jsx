import React, { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import 'highlight.js/styles/github-dark.css';
import './styles.css';
import {
  conversationKey,
  connectionInfo,
  conversationStreamUrl,
  createConversation,
  deleteConversation,
  displayConversationName,
  markConversationSeen,
  loadConversations,
  loadMessageRange,
  loadMessages,
  loadModels,
  loadStatus,
  loadWorkspace,
  loadWorkspaceFile,
  postConversationMessage,
  renameConversation
} from './lib/api';
import { ConversationBar } from './components/ConversationBar';
import { WindowChrome } from './components/WindowChrome';
import { ChatWorkspace } from './components/ChatWorkspace';
import { OverviewPanel } from './components/OverviewPanel';
import { WorkspacePanel } from './components/WorkspacePanel';
import { FilePreviewPanel } from './components/FilePreviewPanel';
import { TerminalDock } from './components/TerminalDock';
import { NewConversationDialog } from './components/NewConversationDialog';
import { SettingsDialog } from './components/SettingsDialog';
import { clamp, formatBytes, formatModel, statusUsageTotals } from './lib/format';
import { fileExtension, fileNameFromPath, imageMimeType } from './lib/fileUtils';
import { activityFromMessages, addUsageTotals, firstMessageId, hasOlderMessages, isFinalAssistantMessage, lastMessageId, lastServerMessageId, liveActivitySignature, mergeMessages, shortText, usageDeltaFromMessages, websocketUrl } from './lib/messageUtils';
import { collectDroppedFiles, packFilesToTarGz, uploadPayloadStats } from './lib/uploadArchive';
import { normalizeWorkspacePath, parentWorkspacePath, workspaceEntryKind, workspaceFileKind } from './lib/workspaceUtils';

const SIDEBAR_EXPANDED = 286;
const SIDEBAR_COLLAPSED = 0;
const WORKSPACE_PANEL_MIN = 340;
const WORKSPACE_PANEL_MAX = 620;
const TERMINAL_HEIGHT_MIN = 160;
const TERMINAL_HEIGHT_MAX = 620;
const TERMINAL_LIST_MIN = 180;
const TERMINAL_LIST_MAX = 360;
const MAX_UPLOAD_COMPRESSED_BYTES = 10 * 1024 * 1024;

function setPxVariable(element, name, value) {
  if (!Number.isFinite(value)) return;
  element.style.setProperty(name, `${value}px`);
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

function maxMessageId(...values) {
  let max = -1;
  for (const value of values) {
    const number = Number(value);
    if (Number.isFinite(number)) max = Math.max(max, number);
  }
  return max >= 0 ? String(max) : undefined;
}

function compareMessageIds(left, right) {
  const leftNumber = Number(left);
  const rightNumber = Number(right);
  if (!Number.isFinite(leftNumber) && !Number.isFinite(rightNumber)) return 0;
  if (!Number.isFinite(leftNumber)) return -1;
  if (!Number.isFinite(rightNumber)) return 1;
  return leftNumber === rightNumber ? 0 : leftNumber > rightNumber ? 1 : -1;
}

function mergeConversationSummary(existing, incoming) {
  if (!existing) return incoming;
  if (!incoming) return existing;
  const incomingHasNewerMessage = compareMessageIds(incoming.last_message_id, existing.last_message_id) >= 0;
  const seen = maxMessageId(existing.last_seen_message_id, incoming.last_seen_message_id);
  const incomingSeen = Number(incoming?.last_seen_message_id);
  const existingSeen = Number(existing?.last_seen_message_id);
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
    last_seen_at: Number.isFinite(incomingSeen) && (!Number.isFinite(existingSeen) || incomingSeen >= existingSeen)
      ? incoming?.last_seen_at
      : existing?.last_seen_at
  };
}

function applyConversationStreamEvent(current, payload, { serverId, hiddenConversations = {} } = {}) {
  const hidden = (conversation) => hiddenConversations[conversationKey(serverId, conversation.conversation_id)];
  const sort = (list) => [...list].sort((left, right) => left.conversation_id.localeCompare(right.conversation_id));
  const upsert = (list, incoming) => {
    if (!incoming?.conversation_id) return list;
    if (hidden(incoming)) {
      return list.filter((conversation) => conversation.conversation_id !== incoming.conversation_id);
    }
    const exists = list.some((conversation) => conversation.conversation_id === incoming.conversation_id);
    if (!exists) return sort([...list, incoming]);
    return list.map((conversation) => (
      conversation.conversation_id === incoming.conversation_id
        ? mergeConversationSummary(conversation, incoming)
        : conversation
    ));
  };

  if (payload.type === 'conversation_snapshot') {
    const existingById = new Map(current.map((conversation) => [conversation.conversation_id, conversation]));
    return (payload.conversations || [])
      .filter((conversation) => !hidden(conversation))
      .map((conversation) => mergeConversationSummary(existingById.get(conversation.conversation_id), conversation));
  }

  if (payload.type === 'conversation_upserted') {
    return upsert(current, payload.conversation);
  }

  if (payload.type === 'conversation_processing' && payload.conversation_id) {
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

  if (payload.type === 'conversation_turn_completed' && payload.conversation_id) {
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

  if (payload.type === 'conversation_seen' && payload.conversation_id && payload.seen) {
    return current.map((conversation) => (
      conversation.conversation_id === payload.conversation_id
        ? mergeConversationSummary(conversation, {
          conversation_id: payload.conversation_id,
          last_seen_message_id: payload.seen.last_seen_message_id,
          last_seen_at: payload.seen.updated_at
        })
        : conversation
    ));
  }

  return current;
}

function hasUnreadConversation(conversation) {
  return compareMessageIds(conversation?.last_message_id, conversation?.last_seen_message_id) > 0;
}

function sleep(ms) {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
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

function shouldRetryControlStatus(commandState, status) {
  return commandState?.name === '/model' && Boolean(status?.model_selection_pending);
}

function clearConversationModelSelectionPending(conversation) {
  return conversation
    ? { ...conversation, model_selection_pending: false }
    : conversation;
}

function normalizePlan(rawPlan) {
  const items = Array.isArray(rawPlan)
    ? rawPlan
    : Array.isArray(rawPlan?.plan)
      ? rawPlan.plan
      : Array.isArray(rawPlan?.items)
        ? rawPlan.items
        : [];
  const plan = items
    .map(normalizePlanItem)
    .filter((item) => item.step);
  const explanation = String(rawPlan?.explanation || rawPlan?.summary || '').trim();
  return (explanation || plan.length) ? { explanation, plan } : null;
}

function normalizePlanItem(item) {
  const rawStep = String(item?.step || item?.text || item?.title || '').trim();
  const marker = rawStep.match(/^\[(x|~|\s*)]\s*/i)?.[1]?.toLowerCase();
  const markerStatus = marker === 'x'
    ? 'completed'
    : marker === '~'
      ? 'in_progress'
      : marker !== undefined
        ? 'pending'
        : '';
  const rawStatus = normalizePlanStatus(String(item?.status || item?.state || '').trim().toLowerCase());
  const status = markerStatus && (!rawStatus || rawStatus === 'pending')
    ? markerStatus
    : rawStatus || markerStatus || 'pending';
  return {
    step: rawStep.replace(/^\[(?:x|~|\s*)]\s*/i, '').trim(),
    status
  };
}

function normalizePlanStatus(status) {
  if (status === 'completed' || status === 'done' || status === 'success') return 'completed';
  if (status === 'in_progress' || status === 'running' || status === 'active') return 'in_progress';
  if (status === 'pending' || status === 'todo') return 'pending';
  return '';
}

function planFromProgressText(text) {
  const value = String(text || '');
  const [, rawPlan = ''] = value.split(/\n\n计划\n?/);
  if (!rawPlan.trim()) return null;
  const plan = rawPlan
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const marker = line.slice(0, 1);
      const status = marker === '☑' ? 'completed' : marker === '◐' ? 'in_progress' : 'pending';
      return { status, step: line.replace(/^[☐◐☑]\s*/, '').trim() };
    })
    .filter((item) => item.step);
  return plan.length ? { explanation: '', plan } : null;
}

function progressActivityFromText(text) {
  const value = String(text || '').split(/\n\n计划\n?/)[0] || '';
  const stage = value.match(/(?:阶段|状态)：\s*([^\n]+)/)?.[1];
  if (stage) return stage.replace(/[.。]+$/, '').trim();
  if (/已完成/.test(value)) return '已完成';
  if (/失败/.test(value)) return '执行失败';
  return '';
}

function normalizeProgressFeedback(payload) {
  const source = payload.progress || payload.event || payload;
  const finalState = source.final_state || source.finalState || payload.final_state || payload.finalState || '';
  const phase = source.phase || payload.phase || '';
  const state = finalState === 'failed' || phase === 'failed'
    ? 'failed'
    : finalState === 'done' || phase === 'done'
      ? 'done'
      : 'running';
  const plan = normalizePlan(source.plan || source.task_plan || source.taskPlan || payload.plan)
    || planFromProgressText(source.text || payload.text);
  const activity = String(
    source.activity
    || source.stage
    || source.phase
    || source.status
    || progressActivityFromText(source.text || payload.text)
    || (state === 'done' ? '已完成' : state === 'failed' ? '执行失败' : '思考中')
  ).trim();
  const model = String(source.model || source.model_name || source.modelName || '').trim();
  return {
    id: `progress-${source.turn_id || source.turnId || payload.turn_id || 'current'}`,
    title: state === 'failed' ? '执行失败' : state === 'done' ? '执行完毕' : '思考中',
    detail: activity,
    activity,
    model,
    plan,
    state
  };
}

function mergeProgressActivity(current, progress) {
  const existing = current.find((item) => item.id === progress.id);
  return {
    ...existing,
    ...progress,
    plan: progress.plan || existing?.plan || null,
    model: progress.model || existing?.model || ''
  };
}

function App() {
  const [settings, setSettings] = useState(null);
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
  const [draft, setDraft] = useState('');
  const [openFiles, setOpenFiles] = useState([]);
  const [activeFilePath, setActiveFilePath] = useState('');
  const [appForeground, setAppForeground] = useState(() => (
    typeof document === 'undefined'
      ? true
      : document.visibilityState === 'visible' && document.hasFocus()
  ));
  const messagesRef = useRef([]);
  const conversationsRef = useRef([]);
  const appForegroundRef = useRef(appForeground);
  const selectedRef = useRef(null);
  const websocketRef = useRef(null);
  const websocketReconnectRef = useRef(null);
  const websocketKeyRef = useRef('');
  const seenUsageMessagesRef = useRef(new Map());
  const loadingOlderRef = useRef(false);
  const layoutDraftRef = useRef(null);
  const restoringUiRef = useRef(false);
  const uiSaveTimerRef = useRef(null);
  const readSaveTimersRef = useRef(new Map());

  useEffect(() => {
    messagesRef.current = messages;
  }, [messages]);

  useEffect(() => {
    conversationsRef.current = conversations;
  }, [conversations]);

  useEffect(() => {
    appForegroundRef.current = appForeground;
  }, [appForeground]);

  useLayoutEffect(() => {
    applyChromeMetrics(window.stellacode2?.chromeMetrics?.());
  }, []);

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
    selectedRef.current = selected;
  }, [selected]);

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

  const layoutValues = conversationLayout || settings?.layout || {};
  const sidebarWidth = sidebarMode === 'collapsed' ? SIDEBAR_COLLAPSED : clamp(layoutValues.sidebar, 220, 520) || SIDEBAR_EXPANDED;
  const overviewPanelWidth = clamp(layoutValues.inspector, 320, 760) || 420;
  const workspacePanelWidth = clamp(layoutValues.file, WORKSPACE_PANEL_MIN, WORKSPACE_PANEL_MAX) || 360;
  const previewPanelWidth = clamp(layoutValues.preview, 320, 820) || 480;
  const terminalHeight = clamp(layoutValues.terminal, TERMINAL_HEIGHT_MIN, TERMINAL_HEIGHT_MAX) || 240;
  const terminalListWidth = clamp(layoutValues.terminalList, TERMINAL_LIST_MIN, TERMINAL_LIST_MAX) || 210;
  const previewPanelRight = workspacePanelOpen ? workspacePanelWidth : 0;
  const overviewPanelRight = previewPanelRight + (previewPanelOpen ? previewPanelWidth : 0);
  const rightContentInset = (overviewPanelOpen ? overviewPanelWidth : 0) + (workspacePanelOpen ? workspacePanelWidth : 0) + (previewPanelOpen ? previewPanelWidth : 0);
  const activeConversation = useMemo(
    () => conversations.find((item) => item.conversation_id === selected?.conversationId) || null,
    [conversations, selected]
  );
  const selectedKey = selected ? conversationKey(selected.serverId, selected.conversationId) : '';
  const selectedStatus = selected ? statuses.get(selectedKey) : null;
  const settingsReady = Boolean(settings);
  const composerMode = useMemo(
    () => composerModeInfo(selectedStatus),
    [selectedStatus]
  );
  const selectedUsage = useMemo(
    () => statusUsageTotals(selectedStatus, selectedKey ? statusDeltas.get(selectedKey) : null),
    [selectedStatus, selectedKey, statusDeltas]
  );
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
    const saved = await window.stellacode2.saveSettings(next);
    const merged = {
      ...saved,
      layout: next?.layout ? { ...(saved.layout || {}), ...(next.layout || {}) } : saved.layout,
      conversationUi: next?.conversationUi ? { ...(saved.conversationUi || {}), ...(next.conversationUi || {}) } : saved.conversationUi
    };
    setSettings(merged);
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
        window.stellacode2.saveSettings(next).catch(() => {});
      }, 260);
      return next;
    });
  }, []);

  const markConversationRead = useCallback((serverId, conversationId, lastMessageId) => {
    const seen = Number(lastMessageId);
    if (!appForegroundRef.current) return;
    if (!serverId || !conversationId || !Number.isFinite(seen)) return;
    const key = conversationKey(serverId, conversationId);
    const nextConversations = conversationsRef.current.map((conversation) => (
      conversation.conversation_id === conversationId
        ? mergeConversationSummary(conversation, {
          conversation_id: conversationId,
          last_seen_message_id: String(seen)
        })
        : conversation
    ));
    conversationsRef.current = nextConversations;
    setConversations(nextConversations);
    const existing = readSaveTimersRef.current.get(key);
    if (existing) window.clearTimeout(existing);
    const timer = window.setTimeout(() => {
      readSaveTimersRef.current.delete(key);
      markConversationSeen(serverId, conversationId, seen).catch(() => {});
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

  const refreshConversations = useCallback(async (serverId, sourceSettings = null) => {
    if (!serverId) return;
    setLoading(true);
    try {
      const list = await loadConversations(serverId);
      const hidden = sourceSettings?.hiddenConversations || {};
      const visibleList = list.filter((conversation) => !hidden[conversationKey(serverId, conversation.conversation_id)]);
      setConversations(visibleList);
      if (!selectedRef.current && visibleList[0]) {
        setSelected({ serverId, conversationId: visibleList[0].conversation_id });
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
    const streamContext = {
      serverId: activeServerId,
      hiddenConversations: settings?.hiddenConversations || {}
    };
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
          const nextConversations = applyConversationStreamEvent(conversationsRef.current, payload, streamContext);
          conversationsRef.current = nextConversations;
          setConversations(nextConversations);
          if (!selectedRef.current) {
            const fallbackConversation = payload.type === 'conversation_snapshot'
              ? (payload.conversations || [])
                .find((conversation) => !streamContext.hiddenConversations[conversationKey(activeServerId, conversation.conversation_id)])
              : payload.conversation;
            if (fallbackConversation?.conversation_id) {
              setSelected({ serverId: activeServerId, conversationId: fallbackConversation.conversation_id });
            }
          }
          if (payload.type === 'conversation_turn_completed' && payload.conversation_id) {
            const completed = nextConversations.find((conversation) => conversation.conversation_id === payload.conversation_id);
            const selectedConversation = selectedRef.current;
            const isActive = selectedConversation?.serverId === activeServerId
              && selectedConversation?.conversationId === payload.conversation_id;
            const isVisibleActive = isActive && appForegroundRef.current;
            if (selectedConversation && completed && !isVisibleActive && hasUnreadConversation(completed)) {
              window.stellacode2?.notify?.({
                title: 'Stellacode',
                body: `${displayConversationName(settings, activeServerId, completed)} 有新消息`
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
  }, [activeServerId, settingsReady, settings?.hiddenConversations]);

  useEffect(() => {
    if (!appForeground) return;
    if (!selectedKey || !activeConversation?.last_message_id) return;
    markConversationRead(selected.serverId, selected.conversationId, activeConversation.last_message_id);
  }, [appForeground, selected, selectedKey, activeConversation?.last_message_id, markConversationRead]);

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
      await refreshConversations(saved.activeServerId, saved);
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
      refreshConversations(loaded.activeServerId, loaded);
    });
  }, [refreshConversations]);

  const renameSelectedConversation = useCallback(async (conversation) => {
    if (!activeServerId || !conversation) return;
    const currentName = displayConversationName(settings, activeServerId, conversation);
    const nextName = window.prompt('重命名 Conversation', currentName);
    if (nextName === null) return;
    const nickname = nextName.trim();
    if (!nickname || nickname === currentName) return;
    try {
      const updated = await renameConversation(activeServerId, conversation.conversation_id, nickname);
      setConversations((current) => current.map((item) => (
        item.conversation_id === conversation.conversation_id
          ? { ...item, ...(updated || {}), nickname }
          : item
      )));
    } catch (error) {
      window.alert(error?.message || '重命名失败');
    }
  }, [activeServerId, settings]);

  const deleteSelectedConversation = useCallback(async (conversation) => {
    if (!activeServerId || !conversation || !settings) return;
    const title = displayConversationName(settings, activeServerId, conversation);
    if (!window.confirm(`删除 Conversation「${title}」？`)) return;
    const key = conversationKey(activeServerId, conversation.conversation_id);
    const nextSettings = {
      ...settings,
      hiddenConversations: {
        ...(settings.hiddenConversations || {}),
        [key]: true
      }
    };
    await saveSettings(nextSettings);
    setConversations((current) => {
      const next = current.filter((item) => item.conversation_id !== conversation.conversation_id);
      if (selected?.conversationId === conversation.conversation_id) {
        setSelected(next[0] ? { serverId: activeServerId, conversationId: next[0].conversation_id } : null);
      }
      return next;
    });
    deleteConversation(activeServerId, conversation.conversation_id).catch((error) => {
      console.warn('server-side conversation delete is unavailable:', error);
    });
  }, [activeServerId, saveSettings, selected?.conversationId, settings]);

  const createNewConversation = useCallback(async ({ serverId, nickname }) => {
    if (!serverId || creatingConversation) return;
    setCreatingConversation(true);
    try {
      const response = await createConversation(serverId, { nickname });
      const conversationId = response?.conversation_id;
      if (!conversationId) throw new Error('创建 Conversation 失败');
      let nextSettings = settings;
      if (settings?.activeServerId !== serverId) {
        nextSettings = await saveSettings({ ...(settings || {}), activeServerId: serverId });
        setActiveServerId(serverId);
      }
      const list = await loadConversations(serverId);
      const hidden = nextSettings?.hiddenConversations || {};
      setConversations(list.filter((conversation) => !hidden[conversationKey(serverId, conversation.conversation_id)]));
      setSelected({ serverId, conversationId });
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

  const fetchWorkspacePath = useCallback(async (path = '', options = {}) => {
    if (!selected) return null;
    const normalized = normalizeWorkspacePath(path);
    if (!options.force && workspaceListings.has(normalized)) {
      return workspaceListings.get(normalized);
    }
    setWorkspaceError('');
    setWorkspaceLoading((current) => new Set(current).add(normalized));
    try {
      const listing = await loadWorkspace(selected.serverId, selected.conversationId, normalized, 500);
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
  }, [selected, workspaceListings]);

  useEffect(() => {
    if (!selected || !settings) {
      setWorkspaceListings(new Map());
      setWorkspaceExpanded(new Set(['']));
      setWorkspaceError('');
      setOpenFiles([]);
      setActiveFilePath('');
      setConversationLayout(null);
      return undefined;
    }
    const key = conversationKey(selected.serverId, selected.conversationId);
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
    setOpenFiles(savedFiles.map((file) => ({ ...file, loading: true })));
    setActiveFilePath(savedActivePath);
    queueMicrotask(() => {
      restoringUiRef.current = false;
    });
    setWorkspaceLoading((current) => new Set(current).add(''));
    loadWorkspace(selected.serverId, selected.conversationId, '', 500)
      .then((listing) => {
        if (disposed) return;
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
      loadWorkspaceFile(selected.serverId, selected.conversationId, file.path)
        .then((loaded) => {
          if (disposed) return;
          const kind = workspaceFileKind(file.path);
          const data = loaded?.encoding === 'base64' && kind === 'image'
            ? `data:${imageMimeType(file.path)};base64,${loaded.data || ''}`
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
  }, [selected?.serverId, selected?.conversationId, settingsReady]);

  useEffect(() => {
    if (!selectedKey || !settings || restoringUiRef.current) return;
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
    queueConversationUiSave(selectedKey, snapshot);
  }, [
    selectedKey,
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

  const openWorkspaceFile = useCallback(async (entry) => {
    if (!selected || !entry) return;
    const path = normalizeWorkspacePath(entry.path);
    setPreviewPanelOpen(true);
    setActiveFilePath(path);
    setOpenFiles((current) => {
      if (current.some((item) => item.path === path)) return current;
      return [...current, { ...entry, path, kind: workspaceFileKind(entry), loading: true }];
    });
    try {
      const file = await loadWorkspaceFile(selected.serverId, selected.conversationId, path);
      const kind = workspaceFileKind(path);
      const data = file?.encoding === 'base64' && kind === 'image'
        ? `data:${imageMimeType(path)};base64,${file.data || ''}`
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
            loading: false
          }
          : item
      )));
    } catch (error) {
      setOpenFiles((current) => current.map((item) => (
        item.path === path ? { ...item, loading: false, error: error?.message || '读取文件失败' } : item
      )));
    }
  }, [selected]);

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
  }, [selected, upsertTransfer, finishTransfer, fetchWorkspacePath]);

  const downloadWorkspaceEntry = useCallback(async (entry) => {
    if (!selected || !entry) return;
    const id = `download-${Date.now()}`;
    const path = normalizeWorkspacePath(entry.path);
    const kind = workspaceEntryKind(entry) === 'directory' ? 'directory' : 'file';
    try {
      upsertTransfer(id, { type: 'download', title: kind === 'file' ? '下载文件' : '下载文件夹', detail: entry.name || path, state: 'running' });
      const result = await window.stellacode2.downloadWorkspace({
        serverId: selected.serverId,
        conversationId: selected.conversationId,
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

  useEffect(() => {
    if (!selected) return;
    const key = conversationKey(selected.serverId, selected.conversationId);
    if (statuses.has(key)) return;
    let disposed = false;
    loadStatus(selected.serverId, selected.conversationId)
      .then((status) => {
        if (disposed) return;
        setStatuses((prev) => new Map(prev).set(key, status));
      })
      .catch(() => {});
    return () => {
      disposed = true;
    };
  }, [selected, statuses]);

  useEffect(() => {
    if (!selected) return;
    const key = conversationKey(selected.serverId, selected.conversationId);
    let disposed = false;
    let reconnectTimer = null;

    const closeSocket = () => {
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      if (websocketReconnectRef.current) {
        clearTimeout(websocketReconnectRef.current);
        websocketReconnectRef.current = null;
      }
      const socket = websocketRef.current;
      websocketRef.current = null;
      if (socket && socket.readyState <= WebSocket.OPEN) {
        socket.close();
      }
    };

    const applyIncomingMessages = (incoming) => {
      if (!Array.isArray(incoming) || incoming.length === 0 || disposed || websocketKeyRef.current !== key) return;
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
        return next;
      });
      const latestId = incoming.reduce((max, message) => {
        const id = Number(message?.id ?? message?.message_id);
        return Number.isFinite(id) ? Math.max(max, id) : max;
      }, -1);
      if (latestId >= 0) {
        setConversations((current) => current.map((conversation) => (
          conversation.conversation_id === selected.conversationId
            ? { ...conversation, last_message_id: String(latestId), message_count: Math.max(Number(conversation.message_count || 0), latestId + 1) }
            : conversation
        )));
        markConversationRead(selected.serverId, selected.conversationId, latestId);
      }
      const activity = activityFromMessages(incoming);
      if (activity) setSessionActivity(activity);
      if (incoming.some((message) => isFinalAssistantMessage(message))) {
        setTimeout(() => {
          if (!disposed && websocketKeyRef.current === key) {
            setRunningActivities([]);
          }
        }, 700);
      }
    };

    const reconcileAck = async (ack) => {
      const nextId = String(ack?.next_message_id || '');
      if (!nextId || disposed || websocketKeyRef.current !== key) return;
      const current = messagesRef.current;
      const lastId = lastMessageId(current);
      if (!lastId) {
        const currentId = String(ack?.current_message_id || '');
        if (!currentId) {
          messagesRef.current = [];
          setMessages([]);
          setMessagesReady(true);
          return;
        }
        const initial = await loadMessageRange(selected.serverId, selected.conversationId, currentId, {
          direction: 'before',
          includeAnchor: true,
          limit: 40
        });
        if (!disposed && websocketKeyRef.current === key) {
          messagesRef.current = initial;
          setMessages(initial);
          setMessagesReady(true);
        }
        return;
      }
      if (Number(nextId) > Number(lastId) + 1) {
        const gap = Math.min(200, Number(nextId) - Number(lastId) - 1);
        const missing = await loadMessageRange(selected.serverId, selected.conversationId, lastId, {
          direction: 'after',
          includeAnchor: false,
          limit: gap
        });
        applyIncomingMessages(missing);
      }
    };

    const connect = async () => {
      try {
        const info = await connectionInfo(selected.serverId);
        if (disposed || websocketKeyRef.current !== key) return;
        const socket = new WebSocket(websocketUrl(info.baseUrl, info.token));
        websocketRef.current = socket;
        socket.addEventListener('open', () => {
          socket.send(JSON.stringify({ type: 'subscribe_foreground', conversation_id: selected.conversationId }));
        });
        socket.addEventListener('message', (event) => {
          let payload;
          try {
            payload = JSON.parse(event.data);
          } catch {
            return;
          }
          if (payload.type === 'subscription_ack') {
            setSessionActivity(payload.reason === 'session_changed' ? 'Session 已切换' : '实时连接已同步');
            reconcileAck(payload).catch(() => {});
          } else if (payload.type === 'messages') {
            applyIncomingMessages(payload.messages || []);
          } else if (payload.type === 'processing') {
            setConversations((current) => current.map((conversation) => (
              conversation.conversation_id === selected.conversationId
                ? {
                  ...conversation,
                  processing_state: payload.state,
                  running: payload.state === 'typing'
                }
                : conversation
            )));
            if (payload.state === 'typing') {
              setSessionActivity('正在思考');
              updateRunningActivities((current) => [
                ...current.filter((item) => item.id !== 'thinking'),
                {
                  id: 'thinking',
                  title: '思考中',
                  detail: '模型正在组织下一步操作',
                  state: 'running',
                  plan: current.findLast((item) => item.plan)?.plan || null,
                  model: current.findLast((item) => item.model)?.model || ''
                }
              ]);
            } else {
              setSessionActivity('');
            }
          } else if (payload.type === 'progress_feedback' || payload.type === 'turn_progress') {
            const progress = normalizeProgressFeedback(payload);
            setSessionActivity(progress.state === 'done' ? '已完成' : progress.detail || progress.title);
            if (progress.state === 'done' || progress.state === 'failed') {
              updateRunningActivities((current) => [
                ...current.filter((item) => item.id !== progress.id && item.id !== 'thinking'),
                mergeProgressActivity(current, progress)
              ]);
              setTimeout(() => {
                if (!disposed && websocketKeyRef.current === key) {
                  setRunningActivities([]);
                }
              }, 900);
            } else {
              updateRunningActivities((current) => [
                ...current.filter((item) => item.id !== progress.id && item.id !== 'thinking'),
                mergeProgressActivity(current, progress)
              ]);
            }
          } else if (payload.type === 'status' && payload.status) {
            setStatuses((prev) => new Map(prev).set(key, payload.status));
            seenUsageMessagesRef.current.set(key, new Set());
            setStatusDeltas((current) => {
              const next = new Map(current);
              next.delete(key);
              return next;
            });
          } else if (payload.type === 'error') {
            setSessionActivity(payload.message || payload.error || '实时连接错误');
          }
        });
        socket.addEventListener('close', () => {
          if (disposed || websocketKeyRef.current !== key) return;
          reconnectTimer = setTimeout(connect, 2000);
          websocketReconnectRef.current = reconnectTimer;
          setSessionActivity('实时连接异常，正在重连');
        });
        socket.addEventListener('error', () => {
          if (!disposed) setSessionActivity('实时连接异常');
        });
      } catch {
        if (!disposed) setSessionActivity('实时连接不可用，使用刷新兜底');
        loadMessages(selected.serverId, selected.conversationId)
          .then((initial) => {
            if (disposed || websocketKeyRef.current !== key) return;
            messagesRef.current = initial;
            setMessages(initial);
            setMessagesReady(true);
          })
          .catch(() => {
            if (!disposed) {
              setMessages([]);
              setMessagesReady(true);
            }
          });
      }
    };

    closeSocket();
    websocketKeyRef.current = key;
    messagesRef.current = [];
    setMessages([]);
    setMessagesReady(false);
    setSessionActivity('');
    setRunningActivities([]);
    connect();

    return () => {
      disposed = true;
      if (websocketKeyRef.current === key) websocketKeyRef.current = '';
      closeSocket();
    };
  }, [selected, markConversationRead]);

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
      if (selectedKey && kind !== 'sidebar') {
        setConversationLayout(latestLayout);
      } else {
        setSettings((prev) => prev ? { ...prev, layout: { ...(prev.layout || {}), ...latestLayout } } : prev);
      }
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
    const key = conversationKey(selected.serverId, selected.conversationId);
    const anchorId = firstMessageId(messagesRef.current);
    if (!anchorId) return false;
    loadingOlderRef.current = true;
    try {
      const older = await loadMessageRange(selected.serverId, selected.conversationId, anchorId, {
        direction: 'before',
        includeAnchor: false,
        limit: 40
      });
      if (websocketKeyRef.current !== key || !older.length) return false;
      setMessages((current) => {
        const next = mergeMessages(current, older);
        messagesRef.current = next;
        return next;
      });
      return true;
    } finally {
      loadingOlderRef.current = false;
    }
  }, [selected]);

  const loadAvailableModels = useCallback(async () => {
    if (!selected?.serverId) return [];
    return loadModels(selected.serverId);
  }, [selected?.serverId]);

  const sendMessage = useCallback(async (text) => {
    const value = String(text || '').trim();
    if (!value || !selected || sending) return;
    const key = conversationKey(selected.serverId, selected.conversationId);
    const previousDraft = draft;
    const commandState = slashCommandState(value);
    const previousLastServerId = lastServerMessageId(messagesRef.current);
    const optimistic = {
      id: `local-${Date.now()}`,
      role: 'user',
      user_name: 'Stellacode',
      text: value,
      preview: value,
      message_time: new Date().toISOString(),
      _optimistic: true,
      pending: true
    };
    setDraft('');
    setSending(true);
    setSessionActivity('正在发送');
    updateRunningActivities(() => [
      { id: 'sending', title: '发送中', detail: shortText(value), state: 'running' }
    ]);
    setMessages((current) => {
      const next = [...current, optimistic];
      messagesRef.current = next;
      return next;
    });
    try {
      await postConversationMessage(selected.serverId, selected.conversationId, value);
      if (websocketKeyRef.current !== key) return;
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
        (async () => {
          for (let attempt = 0; attempt < 12; attempt += 1) {
            if (attempt > 0) await sleep(200);
            const status = await loadStatus(selected.serverId, selected.conversationId);
            if (websocketKeyRef.current !== key) return;
            setStatuses((prev) => new Map(prev).set(key, status));
            if (commandState.name === '/model') {
              setConversations((current) => current.map((conversation) => (
                conversation.conversation_id === selected.conversationId
                  ? clearConversationModelSelectionPending({
                      ...conversation,
                      model: status?.model || conversation.model
                    })
                  : conversation
              )));
            }
            seenUsageMessagesRef.current.set(key, new Set());
            setStatusDeltas((current) => {
              const next = new Map(current);
              next.delete(key);
              return next;
            });
            if (!shouldRetryControlStatus(commandState, status)) return;
          }
        })().catch(() => {});
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
      if (!websocketRef.current || websocketRef.current.readyState !== WebSocket.OPEN) {
        const anchorId = previousLastServerId || lastServerMessageId(messagesRef.current);
        const incoming = anchorId
          ? await loadMessageRange(selected.serverId, selected.conversationId, anchorId, {
            direction: 'after',
            includeAnchor: false,
            limit: 80
          })
          : await loadMessages(selected.serverId, selected.conversationId);
        if (websocketKeyRef.current === key) {
          setMessages((current) => {
            const next = mergeMessages(current, incoming);
            messagesRef.current = next;
            return next;
          });
        }
      }
    } catch (error) {
      if (websocketKeyRef.current === key) {
        setDraft((current) => current || previousDraft);
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
    } finally {
      if (websocketKeyRef.current === key) setSending(false);
    }
  }, [draft, selected, sending]);

  const title = activeConversation
    ? displayConversationName(settings, selected.serverId, activeConversation)
    : 'Stellacode';
  const subtitle = activeConversation
    ? [activeConversation.nickname || activeConversation.platform_chat_id, formatModel(activeConversation, selectedStatus), sessionActivity].filter(Boolean).join(' · ')
    : '选择或创建一个 Conversation';

  return (
    <div
      className={`app-root sidebar-${sidebarMode}${rightContentInset ? ' right-panel-open' : ''}${workspacePanelOpen ? ' workspace-panel-open' : ''}${previewPanelOpen ? ' preview-panel-open' : ''}${terminalOpen ? ' terminal-open' : ''}`}
      data-theme={settings?.themeMode || 'system'}
      style={{
        '--sidebar-width': `${sidebarWidth}px`,
        '--overview-panel-width': `${overviewPanelWidth}px`,
        '--overview-panel-right': `${overviewPanelRight}px`,
        '--workspace-panel-width': `${workspacePanelWidth}px`,
        '--preview-panel-width': `${previewPanelWidth}px`,
        '--preview-panel-right': `${previewPanelRight}px`,
        '--terminal-height': `${terminalHeight}px`,
        '--terminal-list-width': `${terminalListWidth}px`,
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
        settings={settings}
        serverId={activeServerId}
        sidebarMode={sidebarMode}
        conversations={conversations}
        statuses={statuses}
        selected={selected}
        loading={loading}
        onSelect={setSelected}
        onOpenSettings={() => setSettingsOpen(true)}
        onRename={renameSelectedConversation}
        onDelete={deleteSelectedConversation}
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
          conversationKey={selected ? conversationKey(selected.serverId, selected.conversationId) : ''}
          modelSelectionPending={selectedStatus?.model_selection_pending ?? Boolean(activeConversation?.model_selection_pending)}
          messages={messages}
          messagesReady={messagesReady}
          draft={draft}
          setDraft={setDraft}
          mode={composerMode}
          hasOlder={hasOlderMessages(messages)}
          onLoadOlder={loadOlderMessages}
          onSend={sendMessage}
          onLoadModels={loadAvailableModels}
          sending={sending}
          runningActivities={runningActivities}
        />
      </main>
      <OverviewPanel
        open={overviewPanelOpen}
        conversation={activeConversation}
        status={selectedStatus}
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
        status={selectedStatus}
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
        onCloseFile={(path) => {
          setOpenFiles((items) => {
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

createRoot(document.getElementById('root')).render(<App />);
