import { normalizeWorkspacePath } from './workspaceUtils';

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

export function normalizeAbsolutePath(value) {
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

export function workspaceTargetFromLocalLink(rawHref, fallbackConversationId, workspaceRoots = []) {
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

export function attachmentConversationFileTarget(attachment, rawUrl, fallbackConversationId, workspaceRoots = []) {
  const absoluteTarget = conversationFileTargetFromPath(localAttachmentPath(attachment, rawUrl));
  if (absoluteTarget) return absoluteTarget;
  const path = attachmentWorkspacePath(attachment, rawUrl, workspaceRoots);
  if (!path || !fallbackConversationId) return null;
  return { conversationId: fallbackConversationId, path };
}

export function attachmentCacheKey(serverId, conversationId, path, attachment, rawUrl = '') {
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
