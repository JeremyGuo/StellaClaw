import { FileArchive, FileCode, FileImage, FileJson, FileText, Folder } from 'lucide-react';
import { fileExtension, isImageFile, isMarkdownFile } from './fileUtils';

export function normalizeWorkspacePath(value = '') {
  return String(value || '')
    .replace(/\\/g, '/')
    .replace(/^\/+/, '')
    .replace(/\/+/g, '/')
    .replace(/\/$/, '');
}

export function parentWorkspacePath(value = '') {
  const normalized = normalizeWorkspacePath(value);
  const parts = normalized.split('/').filter(Boolean);
  parts.pop();
  return parts.join('/');
}

export function joinWorkspacePath(root, relative) {
  const base = String(root || '').replace(/\/$/, '');
  const rel = normalizeWorkspacePath(relative);
  if (!base) return rel;
  return rel ? `${base}/${rel}` : base;
}

export function remoteWorkspaceRoot(listing, status) {
  if (listing?.remote?.cwd) return listing.remote.cwd;
  const remote = String(status?.remote || '');
  const match = remote.match(/`[^`]+`\s+`([^`]*)`/);
  return match?.[1] || '';
}

export function workspaceDisplayRoot(listing, status) {
  return remoteWorkspaceRoot(listing, status) || listing?.workspace_root || status?.workspace || '';
}

export function workspaceAbsolutePath(listing, status, relative) {
  return joinWorkspacePath(workspaceDisplayRoot(listing, status), relative);
}

export function workspaceEntryKind(entry) {
  return String(entry?.kind || '').toLowerCase();
}

export function workspaceEntryIcon(entry) {
  const kind = workspaceEntryKind(entry);
  const name = entry?.name || entry?.path || '';
  const ext = fileExtension(name);
  if (kind === 'directory') return <Folder size={14} />;
  if (kind === 'symlink') return <span className="file-tree-link-icon">↪</span>;
  if (isImageFile(name)) return <FileImage size={14} />;
  if (['zip', 'gz', 'tgz', 'tar', 'rar', '7z'].includes(ext)) return <FileArchive size={14} />;
  if (['json', 'jsonl'].includes(ext)) return <FileJson size={14} />;
  if (['js', 'jsx', 'ts', 'tsx', 'rs', 'py', 'c', 'cpp', 'h', 'hpp', 'css', 'html', 'sh', 'zsh', 'go', 'java'].includes(ext)) return <FileCode size={14} />;
  return <FileText size={14} />;
}

export function workspaceFileKind(entryOrPath) {
  const name = typeof entryOrPath === 'string' ? entryOrPath : entryOrPath?.name || entryOrPath?.path || '';
  if (isImageFile(name)) return 'image';
  if (isMarkdownFile(name)) return 'markdown';
  return 'code';
}
