import { fileNameFromPath } from './fileUtils';
import { normalizeWorkspacePath, workspaceFileKind } from './workspaceUtils';

export function fileTabSnapshot(file) {
  const path = normalizeWorkspacePath(file?.path);
  if (!path) return null;
  return {
    path,
    name: file?.name || fileNameFromPath(path),
    kind: file?.kind || workspaceFileKind(path)
  };
}
