import { useCallback, useMemo, useRef, useState } from 'react';
import { clamp } from '../lib/format';

const SIDEBAR_EXPANDED = 286;
const SIDEBAR_COLLAPSED = 0;
const WORKSPACE_PANEL_MIN = 340;
const WORKSPACE_PANEL_MAX = 620;
const TERMINAL_HEIGHT_MIN = 160;
const TERMINAL_HEIGHT_MAX = 620;
const TERMINAL_LIST_MIN = 180;
const TERMINAL_LIST_MAX = 360;

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

export function layoutSnapshotFromValues(values = {}) {
  return {
    inspector: clamp(values.inspector, 320, 760) || 420,
    file: clamp(values.file, WORKSPACE_PANEL_MIN, WORKSPACE_PANEL_MAX) || 360,
    preview: clamp(values.preview, 320, 820) || 480,
    terminal: clamp(values.terminal, TERMINAL_HEIGHT_MIN, TERMINAL_HEIGHT_MAX) || 240,
    terminalList: clamp(values.terminalList, TERMINAL_LIST_MIN, TERMINAL_LIST_MAX) || 210
  };
}

export function useAppLayout({ settings, setSettings, saveSettings, selectedKey }) {
  const [sidebarMode, setSidebarMode] = useState('expanded');
  const [overviewPanelOpen, setOverviewPanelOpen] = useState(false);
  const [workspacePanelOpen, setWorkspacePanelOpen] = useState(false);
  const [previewPanelOpen, setPreviewPanelOpen] = useState(false);
  const [terminalOpen, setTerminalOpen] = useState(false);
  const [conversationLayout, setConversationLayout] = useState(null);
  const layoutDraftRef = useRef(null);

  const layoutValues = useMemo(() => {
    const globalLayoutValues = settings?.layout || {};
    const conversationLayoutValues = conversationLayout || globalLayoutValues;
    const sidebarWidth = sidebarMode === 'collapsed'
      ? SIDEBAR_COLLAPSED
      : clamp(globalLayoutValues.sidebar, 220, 520) || SIDEBAR_EXPANDED;
    const overviewPanelWidth = clamp(conversationLayoutValues.inspector, 320, 760) || 420;
    const workspacePanelWidth = clamp(conversationLayoutValues.file, WORKSPACE_PANEL_MIN, WORKSPACE_PANEL_MAX) || 360;
    const previewPanelWidth = clamp(conversationLayoutValues.preview, 320, 820) || 480;
    const terminalHeight = clamp(conversationLayoutValues.terminal, TERMINAL_HEIGHT_MIN, TERMINAL_HEIGHT_MAX) || 240;
    const terminalListWidth = clamp(conversationLayoutValues.terminalList, TERMINAL_LIST_MIN, TERMINAL_LIST_MAX) || 210;
    const previewPanelRight = workspacePanelOpen ? workspacePanelWidth : 0;
    const overviewPanelRight = previewPanelRight + (previewPanelOpen ? previewPanelWidth : 0);
    const rightContentInset = (overviewPanelOpen ? overviewPanelWidth : 0)
      + (workspacePanelOpen ? workspacePanelWidth : 0)
      + (previewPanelOpen ? previewPanelWidth : 0);
    return {
      sidebarWidth,
      overviewPanelWidth,
      workspacePanelWidth,
      previewPanelWidth,
      terminalHeight,
      terminalListWidth,
      previewPanelRight,
      overviewPanelRight,
      rightContentInset
    };
  }, [conversationLayout, overviewPanelOpen, previewPanelOpen, settings?.layout, sidebarMode, workspacePanelOpen]);

  const toggleSidebar = useCallback(() => {
    const nextMode = sidebarMode === 'collapsed' ? 'expanded' : 'collapsed';
    setSidebarMode(nextMode);
    if (settings) {
      saveSettings({ ...settings, sidebarMode: nextMode }).catch(() => {});
    }
  }, [saveSettings, settings, sidebarMode]);

  const resizeLayout = useCallback((kind, event) => {
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
      sidebar: layoutValues.sidebarWidth,
      inspector: layoutValues.overviewPanelWidth,
      file: layoutValues.workspacePanelWidth,
      preview: layoutValues.previewPanelWidth,
      terminal: layoutValues.terminalHeight,
      terminalList: layoutValues.terminalListWidth
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
        setSettings((prev) => (prev ? { ...prev, layout: { ...(prev.layout || {}), ...latestLayout } } : prev));
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
  }, [
    layoutValues,
    overviewPanelOpen,
    previewPanelOpen,
    saveSettings,
    selectedKey,
    settings,
    setSettings,
    sidebarMode,
    terminalOpen,
    workspacePanelOpen
  ]);

  return {
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
    conversationLayout,
    setConversationLayout,
    toggleSidebar,
    resizeLayout,
    ...layoutValues
  };
}
