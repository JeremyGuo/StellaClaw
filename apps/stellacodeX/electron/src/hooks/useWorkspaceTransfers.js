import { useCallback, useState } from 'react';
import { formatBytes } from '../lib/format';
import { fileNameFromPath } from '../lib/fileUtils';
import { collectDroppedFiles, packFilesToTarGz, uploadPayloadStats } from '../lib/uploadArchive';
import { normalizeWorkspacePath, parentWorkspacePath, workspaceEntryKind } from '../lib/workspaceUtils';

const MAX_UPLOAD_COMPRESSED_BYTES = 10 * 1024 * 1024;
const MESSAGE_IMAGE_PREVIEW_MAX_BYTES = 20 * 1024 * 1024;

export function useWorkspaceTransfers({
  selected,
  fetchWorkspacePath,
  removeWorkspaceResourceCache,
  setWorkspaceListings
}) {
  const [transfers, setTransfers] = useState([]);

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
  }, [selected, upsertTransfer, finishTransfer, fetchWorkspacePath, removeWorkspaceResourceCache, setWorkspaceListings]);

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

  return {
    transfers,
    uploadWorkspaceItems,
    downloadWorkspaceEntry
  };
}
