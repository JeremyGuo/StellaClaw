import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import 'highlight.js/styles/github-dark.css';
import './styles.css';
import {
  conversationKey,
  connectionInfo,
  createConversation,
  deleteConversation,
  displayConversationName,
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
import { activityFromMessages, addUsageTotals, firstMessageId, hasOlderMessages, isFinalAssistantMessage, lastMessageId, lastServerMessageId, liveActivitiesFromMessages, liveActivitySignature, mergeMessages, shortText, usageDeltaFromMessages, websocketUrl } from './lib/messageUtils';
import { collectDroppedFiles, packFilesToTarGz, uploadPayloadStats } from './lib/uploadArchive';
import { normalizeWorkspacePath, parentWorkspacePath, workspaceEntryKind, workspaceFileKind } from './lib/workspaceUtils';

const SIDEBAR_EXPANDED = 286;
const SIDEBAR_COLLAPSED = 0;
const WORKSPACE_PANEL_MIN = 340;
const WORKSPACE_PANEL_MAX = 620;
const MAX_UPLOAD_COMPRESSED_BYTES = 10 * 1024 * 1024;

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
  const [terminalOpen, setTerminalOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsSaving, setSettingsSaving] = useState(false);
  const [newConversationOpen, setNewConversationOpen] = useState(false);
  const [creatingConversation, setCreatingConversation] = useState(false);
  const [draft, setDraft] = useState('');
  const [openFiles, setOpenFiles] = useState([]);
  const [activeFilePath, setActiveFilePath] = useState('');
  const messagesRef = useRef([]);
  const selectedRef = useRef(null);
  const websocketRef = useRef(null);
  const websocketReconnectRef = useRef(null);
  const websocketKeyRef = useRef('');
  const seenUsageMessagesRef = useRef(new Map());
  const loadingOlderRef = useRef(false);

  useEffect(() => {
    messagesRef.current = messages;
  }, [messages]);

  useEffect(() => {
    document.documentElement.dataset.theme = settings?.themeMode || 'system';
  }, [settings?.themeMode]);

  useEffect(() => {
    selectedRef.current = selected;
  }, [selected]);

  const sidebarWidth = sidebarMode === 'collapsed' ? SIDEBAR_COLLAPSED : clamp(settings?.layout?.sidebar, 220, 520) || SIDEBAR_EXPANDED;
  const overviewPanelWidth = clamp(settings?.layout?.inspector, 320, 760) || 420;
  const workspacePanelWidth = clamp(settings?.layout?.file, WORKSPACE_PANEL_MIN, WORKSPACE_PANEL_MAX) || 360;
  const previewPanelWidth = clamp(settings?.layout?.preview, 320, 820) || 480;
  const previewPanelRight = workspacePanelOpen ? workspacePanelWidth : 0;
  const overviewPanelRight = previewPanelRight + (previewPanelOpen ? previewPanelWidth : 0);
  const rightContentInset = (overviewPanelOpen ? overviewPanelWidth : 0) + (workspacePanelOpen ? workspacePanelWidth : 0) + (previewPanelOpen ? previewPanelWidth : 0);
  const activeConversation = useMemo(
    () => conversations.find((item) => item.conversation_id === selected?.conversationId) || null,
    [conversations, selected]
  );
  const selectedKey = selected ? conversationKey(selected.serverId, selected.conversationId) : '';
  const selectedStatus = selected ? statuses.get(selectedKey) : null;
  const selectedUsage = useMemo(
    () => statusUsageTotals(selectedStatus, selectedKey ? statusDeltas.get(selectedKey) : null),
    [selectedStatus, selectedKey, statusDeltas]
  );

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

  const appendRunningActivities = useCallback((incoming) => {
    if (!incoming.length) return;
    updateRunningActivities((current) => {
      const byId = new Map(current.map((item) => [item.id, item]));
      for (const item of incoming) {
        byId.set(item.id, item);
      }
      return Array.from(byId.values());
    });
  }, [updateRunningActivities]);

  const saveSettings = useCallback(async (next) => {
    const saved = await window.stellacode2.saveSettings(next);
    setSettings(saved);
    return saved;
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
    setWorkspaceListings(new Map());
    setWorkspaceExpanded(new Set(['']));
    setWorkspaceError('');
    setOpenFiles([]);
    setActiveFilePath('');
    if (selected) {
      fetchWorkspacePath('', { force: true }).catch(() => {});
    }
  }, [selected?.serverId, selected?.conversationId]);

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
      const activity = activityFromMessages(incoming);
      if (activity) setSessionActivity(activity);
      appendRunningActivities(liveActivitiesFromMessages(incoming));
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
            if (payload.state === 'typing') {
              setSessionActivity('正在思考');
              updateRunningActivities((current) => [
                ...current.filter((item) => item.id !== 'thinking'),
                { id: 'thinking', title: '思考中', detail: '模型正在组织下一步操作', state: 'running' }
              ]);
            } else {
              setSessionActivity('');
              setRunningActivities([]);
            }
          } else if (payload.type === 'progress_feedback') {
            setSessionActivity(payload.final_state === 'done' ? '已完成' : (payload.text || '正在处理'));
            if (payload.final_state === 'done' || payload.final_state === 'failed') {
              updateRunningActivities((current) => [
                ...current.filter((item) => item.id !== `progress-${payload.turn_id || 'current'}`),
                {
                  id: `progress-${payload.turn_id || 'current'}`,
                  title: payload.final_state === 'failed' ? '执行失败' : '执行完毕',
                  detail: shortText(payload.text || ''),
                  state: payload.final_state === 'failed' ? 'failed' : 'done'
                }
              ]);
              setTimeout(() => {
                if (!disposed && websocketKeyRef.current === key) {
                  setRunningActivities([]);
                }
              }, 900);
            } else {
              updateRunningActivities((current) => [
                ...current.filter((item) => item.id !== `progress-${payload.turn_id || 'current'}`),
                {
                  id: `progress-${payload.turn_id || 'current'}`,
                  title: '正在处理',
                  detail: shortText(payload.text || '等待模型状态更新'),
                  state: 'running'
                }
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
  }, [selected]);

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
    const scroll = root?.querySelector('.message-scroll');
    const bottomOffset = scroll ? scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight : 0;
    const startX = event.clientX;
    const startLayout = {
      ...(settings.layout || {}),
      sidebar: sidebarWidth,
      inspector: overviewPanelWidth,
      file: workspacePanelWidth,
      preview: previewPanelWidth
    };
    let latestLayout = startLayout;
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
      setSettings((prev) => prev ? { ...prev, layout: { ...(prev.layout || {}), ...latestLayout } } : prev);
      saveSettings({ ...settings, layout: { ...(settings.layout || {}), ...latestLayout } }).catch(() => {});
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
      setSessionActivity('已发送，等待响应');
      updateRunningActivities((current) => [
        ...current.filter((item) => item.id !== 'sending'),
        { id: 'waiting-response', title: '等待响应', detail: '消息已送达，等待模型开始处理', state: 'running' }
      ]);
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
  }, [selected, sending]);

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
        onToggleOverview={() => setOverviewPanelOpen((value) => !value)}
        onToggleWorkspace={() => setWorkspacePanelOpen((value) => !value)}
        onTogglePreview={() => setPreviewPanelOpen((value) => !value)}
        onToggleTerminal={() => setTerminalOpen((value) => !value)}
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
          mode={selectedStatus?.tool_remote_mode ? 'Remote' : '本地'}
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
      <TerminalDock open={terminalOpen} />
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
