import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
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
  postConversationMessage,
  renameConversation,
  renameForegroundSession,
  selectedForegroundSessionId
} from './lib/api';
import { ConversationBar } from './components/ConversationBar';
import { AppErrorBoundary } from './components/AppErrorBoundary';
import { WindowChrome } from './components/WindowChrome';
import { ChatSessionPane } from './components/ChatSessionPane';
import { OverviewPanel } from './components/OverviewPanel';
import { WorkspacePanel } from './components/WorkspacePanel';
import { FilePreviewPanel } from './components/FilePreviewPanel';
import { TerminalDock } from './components/TerminalDock';
import { NewConversationDialog } from './components/NewConversationDialog';
import { RenameConversationDialog, RenameSessionDialog } from './components/RenameConversationDialog';
import { ConversationPropertiesDialog } from './components/ConversationPropertiesDialog';
import { SettingsDialog } from './components/SettingsDialog';
import { clamp, formatModel, statusUsageTotals } from './lib/format';
import { attachmentName, fileExtension, fileNameFromPath } from './lib/fileUtils';
import { addUsageTotals, firstMessageId, hasOlderMessages, lastServerMessageIndex, liveActivitySignature, mergeMessages, messageIndex, messageOrderFromId, shortText } from './lib/messageUtils';
import {
  applyStreamErrorToMessages,
  createStreamBufferStore,
  createStreamIndexTracker,
  markQueuedUserMessage,
  normalizedStreamEvent,
  streamAssistantDeltaPatch,
  streamErrorPatch,
  streamEventIndex,
  streamMessageId,
  streamReasoningDeltaPatch,
  streamReasoningPartPatch,
  streamToolCallDeltaPatch,
  streamToolResultDonePatch,
  streamTurnCompletedPatch,
  streamTurnStartedPatch
} from './lib/chatStreamDataPlane';
import { createChatStreamFrameQueue } from './lib/chatStreamFrameQueue';
import { startChatSocketClient } from './lib/chatSocketClient';
import { chatAckHistoryPlan, chatSnapshotProjection, incomingMessagesPatch, recentMessagesPatch } from './lib/chatMessagePlane';
import { readMessageCache, removeMessageCache, writeMessageCache } from './lib/chatMessageCache';
import {
  recordChatProtocolDiagnostic,
  shouldRecordChatProtocolDiagnostic,
  summarizeMessagesTail,
  summarizePayload
} from './lib/chatProtocolDiagnostics';
import { recordChatPerf } from './lib/chatPerfMetrics';
import { chatSessionStateIsActive, chatSnapshotState, isActiveSessionState, mergeProgressActivity, normalizeProgressFeedback, recentMessagePageParams } from './lib/chatSessionState';
import { getChatRuntimeSnapshot, setChatRuntimeMessages, setChatRuntimeMessagesReady, setChatRuntimeSending } from './lib/chatRuntimeStore';
import {
  compactMessagesSummary,
  patchActionSummary,
  streamDeltaSummary,
  streamUiCategory,
  streamUiKind
} from './lib/chatDebugSummaries';
import { composerModeInfo, controlCommandActivity, slashCommandState } from './lib/composerCommands';
import {
  applyConversationStreamEvent,
  createLocalForegroundSessionId,
  hasUnreadConversation,
  nextForegroundSessionName,
  patchConversationForegroundSession
} from './lib/conversationState';
import {
  attachmentCacheKey,
  attachmentConversationFileTarget,
  normalizeAbsolutePath,
  workspaceTargetFromLocalLink
} from './lib/localTargets';
import { applyChromeMetrics } from './lib/chromeMetrics';
import { fileTabSnapshot } from './lib/workspaceFileTabs';
import { layoutSnapshotFromValues, useAppLayout } from './hooks/useAppLayout';
import { revokeFilePreviewUrls, useWorkspaceState } from './hooks/useWorkspaceState';
import { useWorkspaceWorkflow, workspaceFileImageDataUrl } from './hooks/useWorkspaceWorkflow';
import { useWorkspaceTransfers } from './hooks/useWorkspaceTransfers';
import { effectiveThemeMode, themeCssVariables } from './lib/theme';
import { workspaceDisplayRoot, workspaceFileKind } from './lib/workspaceUtils';

const MESSAGE_IMAGE_PREVIEW_MAX_BYTES = 20 * 1024 * 1024;
const MIN_DISPLAY_FONT_SIZE = 11;
const MAX_DISPLAY_FONT_SIZE = 18;
const MIN_UI_SCALE = 0.8;
const MAX_UI_SCALE = 1.4;

function workspaceFileAttachmentDataUrl(path, file) {
  const imageUrl = workspaceFileImageDataUrl(path, file);
  if (imageUrl) return imageUrl;
  const ext = fileExtension(path);
  if (!['html', 'htm'].includes(ext)) return '';
  const data = file?.data || file?.content || '';
  if (!data) return '';
  if (file?.encoding === 'base64') {
    return `data:text/html;base64,${String(data).replace(/\s/g, '')}`;
  }
  if (file?.encoding === 'utf8') {
    return `data:text/html;charset=utf-8,${encodeURIComponent(String(data))}`;
  }
  return '';
}

function App() {
  const [settings, setSettings] = useState(null);
  const [systemTheme, setSystemTheme] = useState(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return 'dark';
    return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  });
  const [activeServerId, setActiveServerId] = useState('');
  const [conversations, setConversations] = useState([]);
  const [statuses, setStatuses] = useState(new Map());
  const [selected, setSelected] = useState(null);
  const [loading, setLoading] = useState(false);
  const [sessionActivity, setSessionActivity] = useState('');
  const [chatSessionState, setChatSessionState] = useState({ state: 'idle' });
  const [runningActivities, setRunningActivities] = useState([]);
  const [statusDeltas, setStatusDeltas] = useState(() => new Map());
  const [updaterStatus, setUpdaterStatus] = useState({ state: 'idle' });
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsSaving, setSettingsSaving] = useState(false);
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
  const [selectionReferences, setSelectionReferences] = useState([]);
  const messagesRef = useRef(getChatRuntimeSnapshot().messages);
  const sendingRef = useRef(getChatRuntimeSnapshot().sending);
  const chatSessionStateRef = useRef({ state: 'idle' });
  const conversationsRef = useRef([]);
  const appForegroundRef = useRef(
    typeof document === 'undefined'
      ? true
      : document.visibilityState === 'visible' && document.hasFocus()
  );
  const settingsRef = useRef(null);
  const settingsSaveSeqRef = useRef(0);
  const selectedRef = useRef(null);
  const websocketKeyRef = useRef('');
  const seenUsageMessagesRef = useRef(new Map());
  const attachmentImageUrlCacheRef = useRef(new Map());
  const loadingOlderRef = useRef(false);
  const restoringUiRef = useRef(false);
  const uiSaveTimerRef = useRef(null);
  const readSaveTimersRef = useRef(new Map());
  const foregroundReadTimerRef = useRef(0);
  const selectedSessionId = selectedForegroundSessionId(selected);
  const selectedServerId = selected?.serverId || '';
  const selectedConversationId = selected?.conversationId || '';

  const setMessages = useCallback((updater) => {
    const next = setChatRuntimeMessages(updater);
    messagesRef.current = next;
    return next;
  }, []);

  const setMessagesReady = useCallback((value) => {
    setChatRuntimeMessagesReady(value);
  }, []);

  const setSending = useCallback((value) => {
    const next = setChatRuntimeSending(value);
    sendingRef.current = next;
    return next;
  }, []);

  useEffect(() => {
    chatSessionStateRef.current = chatSessionState;
  }, [chatSessionState]);

  useEffect(() => {
    conversationsRef.current = conversations;
  }, [conversations]);

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
    setSelectionReferences([]);
  }, [selected?.serverId, selected?.conversationId, selectedSessionId]);

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
  const selectedConversationUiKey = selectedKey;
  const legacyConversationUiKey = selected ? conversationKey(selected.serverId, selected.conversationId, 'main') : '';
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
  const {
    workspaceListings,
    setWorkspaceListings,
    workspaceExpanded,
    setWorkspaceExpanded,
    workspaceLoading,
    setWorkspaceLoading,
    workspaceError,
    setWorkspaceError,
    openFiles,
    setOpenFiles,
    activeFilePath,
    setActiveFilePath,
    resetWorkspaceState,
    restoreOpenFileTabs,
    readWorkspaceResourceCache,
    writeWorkspaceResourceCache,
    removeWorkspaceResourceCache,
    loadWorkspaceFileCached
  } = useWorkspaceState();
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
      const dataUrl = workspaceFileAttachmentDataUrl(target.path || attachmentName(attachment), file);
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

  const {
    sidebarMode,
    setSidebarMode,
    overviewPanelOpen,
    setOverviewPanelOpen,
    workspacePanelOpen,
    setWorkspacePanelOpen,
    previewPanelOpen,
    setPreviewPanelOpen,
    terminalOpen,
    setTerminalOpen,
    setConversationLayout,
    sidebarWidth,
    overviewPanelWidth,
    workspacePanelWidth,
    previewPanelWidth,
    terminalHeight,
    terminalListWidth,
    previewPanelRight,
    overviewPanelRight,
    rightContentInset,
    toggleSidebar,
    resizeLayout
  } = useAppLayout({ settings, setSettings, saveSettings, selectedKey });

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

  const scheduleForegroundReadMark = useCallback(() => {
    if (foregroundReadTimerRef.current) {
      window.clearTimeout(foregroundReadTimerRef.current);
      foregroundReadTimerRef.current = 0;
    }
    foregroundReadTimerRef.current = window.setTimeout(() => {
      foregroundReadTimerRef.current = 0;
      const currentSelected = selectedRef.current;
      const sessionId = selectedForegroundSessionId(currentSelected);
      const conversation = conversationsRef.current.find((item) => (
        item.conversation_id === currentSelected?.conversationId
      ));
      const session = foregroundSessions(conversation).find((item) => String(item?.id || 'main') === sessionId) || conversation;
      if (!currentSelected || !session?.last_message_id) return;
      markConversationRead(
        currentSelected.serverId,
        currentSelected.conversationId,
        sessionId,
        session.last_message_id
      );
    }, 240);
  }, [markConversationRead]);

  useEffect(() => {
    const updateForeground = () => {
      const startedAt = typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now();
      const next = document.visibilityState === 'visible' && document.hasFocus();
      const previous = appForegroundRef.current;
      appForegroundRef.current = next;
      recordChatPerf('app.foreground_event', (typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now()) - startedAt, {
        foreground: next,
        previous
      });
      if (next && !previous) {
        window.requestAnimationFrame(() => {
          recordChatPerf('app.foreground_first_frame', (typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now()) - startedAt);
        });
        scheduleForegroundReadMark();
      }
    };
    updateForeground();
    window.addEventListener('focus', updateForeground);
    window.addEventListener('blur', updateForeground);
    document.addEventListener('visibilitychange', updateForeground);
    return () => {
      window.removeEventListener('focus', updateForeground);
      window.removeEventListener('blur', updateForeground);
      document.removeEventListener('visibilitychange', updateForeground);
      if (foregroundReadTimerRef.current) {
        window.clearTimeout(foregroundReadTimerRef.current);
        foregroundReadTimerRef.current = 0;
      }
    };
  }, [scheduleForegroundReadMark]);

  useEffect(() => () => {
    if (foregroundReadTimerRef.current) {
      window.clearTimeout(foregroundReadTimerRef.current);
      foregroundReadTimerRef.current = 0;
    }
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

  const {
    fetchWorkspacePath,
    loadPdfPreviewIntoTab,
    refreshPdfPreview,
    toggleWorkspaceDirectory,
    openWorkspaceFile,
    openWorkspacePathTarget,
    refreshWorkspacePreviewFile,
    resolveMarkdownAsset
  } = useWorkspaceWorkflow({
    selected,
    selectedRef,
    workspaceListings,
    setWorkspaceListings,
    setWorkspaceExpanded,
    setWorkspaceLoading,
    setWorkspaceError,
    setOpenFiles,
    setActiveFilePath,
    setPreviewPanelOpen,
    loadWorkspaceFileCached,
    readWorkspaceResourceCache,
    writeWorkspaceResourceCache
  });

  useEffect(() => {
    if (!selected || !settings) {
      resetWorkspaceState();
      setConversationLayout(null);
      return undefined;
    }
    const key = selectedConversationUiKey;
    const savedUi = settings.conversationUi?.[key]
      || (key !== legacyConversationUiKey ? settings.conversationUi?.[legacyConversationUiKey] : null)
      || {};
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
    restoreOpenFileTabs(savedFiles);
    setActiveFilePath(savedActivePath);
    queueMicrotask(() => {
      restoringUiRef.current = false;
    });
    const rootCacheParts = [selected.serverId, selected.conversationId, '', 500];
    const cachedRootListing = readWorkspaceResourceCache('workspace-listing', rootCacheParts);
    if (cachedRootListing) {
      setWorkspaceListings((current) => new Map(current).set('', cachedRootListing));
    }
    fetchWorkspacePath('', { force: true }).catch(() => {});
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
  }, [selected?.serverId, selected?.conversationId, selectedSessionId, selectedConversationUiKey, legacyConversationUiKey, settingsReady, fetchWorkspacePath, loadPdfPreviewIntoTab, readWorkspaceResourceCache, loadWorkspaceFileCached, resetWorkspaceState, restoreOpenFileTabs]);

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

  const openChatLocalLink = useCallback((href) => {
    if (!selected) return false;
    const target = workspaceTargetFromLocalLink(href, selected.conversationId, messageAttachmentWorkspaceRoots);
    if (target?.path === undefined || target?.path === null) return false;
    openWorkspacePathTarget(target).catch((error) => {
      window.alert(error?.message || '打开本地链接失败');
    });
    return true;
  }, [selected, messageAttachmentWorkspaceRoots, openWorkspacePathTarget]);

  const {
    transfers,
    uploadWorkspaceItems,
    downloadWorkspaceEntry
  } = useWorkspaceTransfers({
    selected,
    fetchWorkspacePath,
    removeWorkspaceResourceCache,
    setWorkspaceListings
  });

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
    let streamFrameQueue = null;

    const cacheMessages = (next) => {
      writeMessageCache(serverId, conversationId, sessionId, next);
    };

    const clearMessageCache = () => {
      removeMessageCache(serverId, conversationId, sessionId);
    };

    const recordProtocol = (kind, details = {}) => {
      const category = typeof details === 'function' ? '' : details?.category;
      if (!shouldRecordChatProtocolDiagnostic(kind, category)) return;
      const resolvedDetails = typeof details === 'function' ? details() : details;
      recordChatProtocolDiagnostic(kind, {
        scopeKey: key,
        serverId,
        conversationId,
        foregroundSessionId: sessionId,
        ...resolvedDetails
      });
    };

    const shouldRecordProtocol = (kind, category = '') => (
      shouldRecordChatProtocolDiagnostic(kind, category)
    );

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
      streamFrameQueue?.flushNow?.();
      const patch = incomingMessagesPatch(messagesRef.current, incoming, key, seenUsageMessagesRef.current);
      if (!patch) return;
      if (patch.protocolMismatches.length > 0) {
        recordProtocol('chat.stream_commit_mismatch', {
          mismatches: patch.protocolMismatches,
          incoming: summarizeMessagesTail(incoming, 12),
          beforeTail: summarizeMessagesTail(messagesRef.current, 12),
          afterTail: summarizeMessagesTail(patch.messages, 12)
        });
        console.warn('stream provisional message differed from durable commit', patch.protocolMismatches);
        setSessionActivity('流式消息和落盘消息不一致，已使用落盘消息');
      }
      if (patch.usageDelta.totalTokens > 0 || patch.usageDelta.cost > 0) {
        setStatusDeltas((current) => {
          const next = new Map(current);
          next.set(key, addUsageTotals(next.get(key), patch.usageDelta));
          return next;
        });
      }
      messagesRef.current = patch.messages;
      cacheMessages(patch.messages);
      setMessages(patch.messages);
      recordProtocol('chat.message_arrived', {
        category: 'replace_ui_element',
        action: 'merge durable messages / refresh messages',
        incoming: compactMessagesSummary(incoming, 5),
        afterTail: compactMessagesSummary(patch.messages, 5)
      });
      updateSelectedSessionSummary(patch.latestMessage, patch.latestId, patch.latestIndex);
      if (patch.activity) setSessionActivity(patch.activity);
      if (patch.finalizedActivities.size > 0) {
        updateRunningActivities((current) => current.filter((item) => !patch.finalizedActivities.has(item.id)));
      }
      if (patch.hasFinalAssistant) {
        setTimeout(() => {
          if (!disposed && websocketKeyRef.current === key) {
            setRunningActivities([]);
          }
        }, 700);
      }
    };

    const replaceWithRecentMessages = (incoming) => {
      if (!Array.isArray(incoming) || incoming.length === 0 || disposed || websocketKeyRef.current !== key) return;
      streamFrameQueue?.flushNow?.();
      const patch = recentMessagesPatch(incoming);
      if (!patch) return;
      messagesRef.current = patch.messages;
      cacheMessages(patch.messages);
      setMessages(patch.messages);
      recordProtocol('chat.message_arrived', {
        category: 'replace_ui_element',
        action: 'replace with recent durable messages',
        incoming: compactMessagesSummary(incoming, 5),
        afterTail: compactMessagesSummary(patch.messages, 5)
      });
      updateSelectedSessionSummary(patch.latestMessage, patch.latestId, patch.latestIndex);
      if (patch.activity) setSessionActivity(patch.activity);
    };

    const streamBuffers = createStreamBufferStore();
    const streamTracker = createStreamIndexTracker(key);
    let lastStreamAuxUpdateAt = 0;

    const expectedStreamIndexFromMessages = (event) => {
      const id = streamMessageId(event);
      if (!id) return undefined;
      const message = [...messagesRef.current].reverse().find((item) => (
        item?._streaming
        && String(item?.role || '').toLowerCase() === 'assistant'
        && String(item?.id ?? item?.message_id ?? '').trim() === id
      ));
      if (!message) return undefined;
      const lastIndex = Number(message._lastStreamEventIndex);
      return Number.isFinite(lastIndex) ? lastIndex + 1 : undefined;
    };

    const acceptStreamEvent = (event) => {
      return streamTracker.accept(event, (expected, received) => {
        recordProtocol('chat.stream_index_gap', {
          expected,
          received,
          firstObserved: streamEventIndex(event),
          event: summarizePayload({ type: streamEventType(event), event }),
          messagesTail: summarizeMessagesTail(messagesRef.current, 12)
        });
        setMessages((current) => {
          const next = applyStreamErrorToMessages(current, {
            ...event,
            error: `non-contiguous stream event: expected index ${expected}, received ${received}`
          });
          messagesRef.current = next;
          return next;
        });
        setSessionActivity('流式消息不连续，已撤销当前临时消息');
      }, expectedStreamIndexFromMessages(event));
    };

    const scopedChatState = (state) => ({ scopeKey: key, ...state });
    const turnIdFromState = (state) => String(
      state?.activeTurnId
      || state?.active_turn_id
      || state?.currentTurnState?.turn_id
      || state?.currentTurnState?.turnId
      || ''
    ).trim();
    const keepOrSetRunningState = (current, eventPayload) => (
      chatSessionStateIsActive(current) && current.scopeKey === key
        ? current
        : scopedChatState({ state: 'running', currentTurnState: eventPayload })
    );
    const applyStreamPatch = (patch, event, type) => {
      if (!patch) {
        recordProtocol('chat.stream', () => ({
          category: 'stream',
          action: 'stream arrived, no render change',
          ...streamDeltaSummary(event)
        }));
        return;
      }
      const category = patch.messages ? streamUiCategory(type, patch) : 'stream';
      const protocolKind = patch.messages ? streamUiKind(category) : 'chat.stream';
      const shouldLogPatch = shouldRecordProtocol(protocolKind, category);
      const beforeTail = patch.messages && shouldLogPatch ? compactMessagesSummary(messagesRef.current, 3) : undefined;
      if (patch.resetStreamState) {
        streamTracker.reset();
        streamBuffers.reset();
        streamFrameQueue?.reset?.();
      }
      if (patch.chatState) {
        const previousState = chatSessionStateRef.current;
        if (patch.forceChatState && patch.chatState.state === 'running') {
          const previousTurnId = turnIdFromState(previousState);
          const nextTurnId = turnIdFromState(patch.chatState);
          if (
            previousState?.scopeKey === key
            && chatSessionStateIsActive(previousState)
            && previousTurnId
            && nextTurnId
            && previousTurnId !== nextTurnId
          ) {
            recordProtocol('chat.turn_start_overlap_warning', {
              previousTurnId,
              nextTurnId,
              event: summarizePayload({ type, event }),
              messagesTail: summarizeMessagesTail(messagesRef.current, 12)
            });
            console.warn('chat protocol warning: stream_turn_start received before previous turn completed', {
              scopeKey: key,
              previousTurnId,
              nextTurnId
            });
          }
        }
        setChatSessionState((current) => {
          const nextState = patch.chatState.state === 'running' && !patch.forceChatState
            ? keepOrSetRunningState(current, patch.chatState.currentTurnState || event)
            : scopedChatState(patch.chatState);
          chatSessionStateRef.current = nextState;
          return nextState;
        });
      }
      if (patch.messages) {
        messagesRef.current = patch.messages;
        if (patch.shouldCache) cacheMessages(patch.messages);
        setMessages(patch.messages);
        recordProtocol(protocolKind, () => ({
          category,
          action: patchActionSummary(type, patch),
          ...streamDeltaSummary(event),
          messageCount: patch.messages.length,
          beforeTail,
          afterTail: compactMessagesSummary(patch.messages, 3)
        }));
      } else {
        recordProtocol(protocolKind, () => ({
          category: 'stream',
          action: patchActionSummary(type, patch),
          ...streamDeltaSummary(event),
          activity: patch.activity,
          chatState: patch.chatState?.state
        }));
      }
      const isHighFrequencyStreamPatch = category === 'append_stream_to_ui';
      const nowMs = Date.now();
      const shouldUpdateAuxState = !isHighFrequencyStreamPatch || nowMs - lastStreamAuxUpdateAt > 200;
      if (shouldUpdateAuxState) lastStreamAuxUpdateAt = nowMs;
      if (patch.activity && shouldUpdateAuxState) setSessionActivity(patch.activity);
      if (patch.runningActivity && shouldUpdateAuxState) {
        const removeIds = new Set(patch.removeActivityIds || []);
        updateRunningActivities((current) => [
          ...current.filter((item) => !removeIds.has(item.id)),
          mergeProgressActivity(current, patch.runningActivity)
        ]);
      }
      if (patch.clearRunningActivitiesDelay) {
        setTimeout(() => {
          if (!disposed && websocketKeyRef.current === key) {
            setRunningActivities([]);
          }
        }, patch.clearRunningActivitiesDelay);
      }
    };

    streamFrameQueue = createChatStreamFrameQueue({
      onFlush: (entries) => {
        if (disposed || websocketKeyRef.current !== key) return;
        entries.forEach(({ kind, event }) => {
          if (kind === 'assistant') {
            applyStreamPatch(
              streamAssistantDeltaPatch(messagesRef.current, event, key, streamBuffers),
              event,
              'stream_assistant_message_delta'
            );
          } else if (kind === 'reasoning') {
            applyStreamPatch(
              streamReasoningDeltaPatch(messagesRef.current, event, key, streamBuffers),
              event,
              'stream_reasoning_summary_delta'
            );
          } else if (kind === 'tool') {
            applyStreamPatch(
              streamToolCallDeltaPatch(messagesRef.current, event),
              event,
              'stream_tool_call_delta'
            );
          }
        });
      }
    });

    const drainStreamFrameQueue = (callback) => {
      if (streamFrameQueue?.drainBefore) {
        streamFrameQueue.drainBefore(callback);
      } else if (typeof callback === 'function') {
        callback();
      }
    };

    const applySessionStream = (rawEvent) => {
      const event = normalizedStreamEvent(rawEvent);
      const type = streamEventType(event);
      if (!type || disposed || websocketKeyRef.current !== key) return;
      recordProtocol('chat.stream', () => ({
        category: 'stream',
        action: 'stream arrived',
        ...streamDeltaSummary(event)
      }));

      if (type === 'turn_started' || type === 'stream_turn_start') {
        applyStreamPatch(streamTurnStartedPatch(event), event, type);
        return;
      }

      if (type === 'turn_completed' || type === 'stream_turn_done') {
        drainStreamFrameQueue(() => {
          applyStreamPatch(streamTurnCompletedPatch(messagesRef.current, event), event, type);
        });
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
        streamFrameQueue?.enqueue('assistant', event);
        return;
      }

      if (type === 'stream_reasoning_summary_part_added') {
        if (!acceptStreamEvent(event)) return;
        applyStreamPatch(streamReasoningPartPatch(event), event, type);
        return;
      }

      if (type === 'stream_reasoning_summary_delta') {
        if (!acceptStreamEvent(event)) return;
        streamFrameQueue?.enqueue('reasoning', event);
        return;
      }

      if (type === 'stream_tool_call_delta') {
        if (!acceptStreamEvent(event)) return;
        streamFrameQueue?.enqueue('tool', event);
        return;
      }

      if (type === 'stream_tool_result_done') {
        if (!acceptStreamEvent(event)) return;
        drainStreamFrameQueue(() => {
          applyStreamPatch(streamToolResultDonePatch(messagesRef.current, event), event, type);
        });
        return;
      }

      if (type === 'stream_error') {
        drainStreamFrameQueue(() => {
          streamTracker.clearForEvent(event);
          applyStreamPatch(streamErrorPatch(messagesRef.current, event), event, type);
        });
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
      streamFrameQueue?.flushNow?.();
      setMessages((current) => {
        const next = current.length ? mergeMessages(current, initial) : initial;
        messagesRef.current = next;
        cacheMessages(next);
        return next;
      });
      setMessagesReady(true);
    };

    const reconcileAck = async (ack) => {
      const plan = chatAckHistoryPlan(messagesRef.current, ack);
      if (plan.kind === 'none' || disposed || websocketKeyRef.current !== key) return;
      if (plan.kind === 'clear') {
        messagesRef.current = [];
        clearMessageCache();
        setMessages([]);
        setMessagesReady(true);
        return;
      }
      const missing = await loadMessages(serverId, conversationId, {
        ...plan.params,
        foregroundSessionId: sessionId
      });
      if (plan.replace) {
        replaceWithRecentMessages(missing);
      } else {
        applyIncomingMessages(missing);
      }
      if (!disposed && websocketKeyRef.current === key) {
        setMessagesReady(true);
      }
    };

    const applyChatSnapshotLiveProjection = (snapshot) => {
      if (!snapshot || disposed || websocketKeyRef.current !== key) return;
      const projection = chatSnapshotProjection(messagesRef.current, snapshot);
      if (!projection) return;
      if (projection.changed) {
        streamFrameQueue?.flushNow?.();
        messagesRef.current = projection.messages;
        if (projection.shouldCache) cacheMessages(projection.messages);
        setMessages(projection.messages);
      }
      if (projection.runningActivities?.length > 0) {
        updateRunningActivities((current) => [
          ...current.filter((item) => !projection.runningActivities.some((activity) => activity.id === item.id) && item.id !== 'thinking'),
          ...projection.runningActivities
        ]);
      }
      if (projection.clearRunningActivities) {
        setRunningActivities([]);
      }
      if (projection.activity) setSessionActivity(projection.activity);
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
          streamFrameQueue?.flushNow?.();
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
      streamFrameQueue?.dispose?.();
      socketClient?.close();
    };
  }, [selectedServerId, selectedConversationId, selectedSessionId, markConversationRead]);

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

  const sendMessage = useCallback(async (text, files = [], selections = []) => {
    const value = String(text || '').trim();
    const outgoingFiles = Array.isArray(files) ? files : [];
    const outgoingSelections = Array.isArray(selections) ? selections : [];
    if ((!value && outgoingFiles.length === 0 && outgoingSelections.length === 0) || !selected || sendingRef.current) return false;
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
  }, [selected, selectedSessionId, activeUserName]);

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
        <ChatSessionPane
          title={title}
          conversationKey={selectedKey}
          modelSelectionPending={Boolean(activeConversation?.model_selection_pending ?? selectedConversationStatus?.model_selection_pending)}
          mode={composerMode}
          onLoadOlder={loadOlderMessages}
          onSend={sendMessage}
          onLoadModels={loadAvailableModels}
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
