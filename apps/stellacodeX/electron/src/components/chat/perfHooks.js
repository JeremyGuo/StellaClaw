import { useLayoutEffect } from 'react';
import { isChatPerfDetailedEnabled, recordChatCommitPerf } from '../../lib/chatPerfMetrics';

function perfNow() {
  return typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now();
}

export function renderCommitStart() {
  return isChatPerfDetailedEnabled() ? perfNow() : 0;
}

export function useRenderCommitPerf(name, startedAt, meta) {
  useLayoutEffect(() => {
    if (!startedAt) return;
    recordChatCommitPerf(name, perfNow() - startedAt, meta);
  });
}
