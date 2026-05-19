import { useCallback, useEffect, useRef } from 'react';
import { loadWorkspace } from '../lib/api';
import { fileExtension, fileNameFromPath, imageMimeType } from '../lib/fileUtils';
import { normalizeWorkspacePath, parentWorkspacePath, workspaceEntryKind, workspaceFileKind } from '../lib/workspaceUtils';
import { revokeFilePreviewUrls } from './useWorkspaceState';

const PDF_PREVIEW_MAX_BYTES = 50 * 1024 * 1024;

export function workspaceFileImageDataUrl(path, file) {
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

function safeDecodeUriComponent(value) {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
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

export function useWorkspaceWorkflow({
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
}) {
  const workspaceListingsRef = useRef(workspaceListings);

  useEffect(() => {
    workspaceListingsRef.current = workspaceListings;
  }, [workspaceListings]);

  const fetchWorkspacePath = useCallback(async (path = '', options = {}) => {
    if (!selected) return null;
    const normalized = normalizeWorkspacePath(path);
    if (!options.force && workspaceListingsRef.current.has(normalized)) {
      return workspaceListingsRef.current.get(normalized);
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
  }, [selected, readWorkspaceResourceCache, writeWorkspaceResourceCache, setWorkspaceError, setWorkspaceListings, setWorkspaceLoading]);

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
  }, [selected, selectedRef, setOpenFiles]);

  const refreshPdfPreview = useCallback((entry, scrollHint) => {
    return loadPdfPreviewIntoTab(entry, { keepExistingPreview: true, scrollHint });
  }, [loadPdfPreviewIntoTab]);

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
  }, [fetchWorkspacePath, setWorkspaceExpanded]);

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
  }, [selected, loadPdfPreviewIntoTab, loadWorkspaceFileCached, setActiveFilePath, setOpenFiles, setPreviewPanelOpen, setWorkspaceListings]);

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

  return {
    fetchWorkspacePath,
    loadPdfPreviewIntoTab,
    refreshPdfPreview,
    toggleWorkspaceDirectory,
    openWorkspaceFile,
    openWorkspacePathTarget,
    refreshWorkspacePreviewFile,
    resolveMarkdownAsset
  };
}
