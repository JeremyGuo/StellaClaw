import {
  streamActivityBaseId,
  streamDeltaText,
  streamEventIndex,
  streamItemId
} from './chatStreamDataPlane';

function canUseAnimationFrame() {
  return (
    typeof window !== 'undefined'
    && typeof window.requestAnimationFrame === 'function'
    && (typeof document === 'undefined' || document.visibilityState === 'visible')
  );
}

function queueKey(kind, event) {
  const baseId = streamActivityBaseId(event) || 'current';
  if (kind === 'reasoning') {
    const summaryIndex = event?.summary_index ?? event?.summaryIndex ?? 0;
    return `${kind}:${baseId}:${summaryIndex}:${streamItemId(event)}`;
  }
  if (kind === 'tool') {
    return `${kind}:${baseId}:${streamItemId(event)}`;
  }
  return `${kind}:${baseId}`;
}

function combinedEvent(event, text, eventIndex) {
  return {
    ...event,
    delta: text,
    text_delta: text,
    textDelta: text,
    in_message_index: eventIndex,
    inMessageIndex: eventIndex
  };
}

function entryHasPendingText(entry) {
  return entry.chunks.some((chunk) => chunk.offset < chunk.text.length);
}

function takeEntryText(entry, maxChars) {
  let budget = maxChars;
  let text = '';
  let event = entry.chunks[0]?.event;
  let completeEventIndex;
  while (budget > 0 && entry.chunks.length > 0) {
    const chunk = entry.chunks[0];
    event = chunk.event;
    const remaining = chunk.text.length - chunk.offset;
    const take = Math.min(remaining, budget);
    text += chunk.text.slice(chunk.offset, chunk.offset + take);
    chunk.offset += take;
    budget -= take;
    if (chunk.offset >= chunk.text.length) {
      completeEventIndex = chunk.index;
      entry.chunks.shift();
    } else {
      break;
    }
  }
  return { event, text, completeEventIndex };
}

export function createChatStreamFrameQueue({
  onFlush,
  fallbackIntervalMs = 16,
  targetCharsPerFrame = Number.MAX_SAFE_INTEGER
} = {}) {
  const pending = new Map();
  let scheduled = null;
  let scheduledBy = '';

  const cancelScheduled = () => {
    if (scheduled === null) return;
    if (scheduledBy === 'raf' && typeof window !== 'undefined' && typeof window.cancelAnimationFrame === 'function') {
      window.cancelAnimationFrame(scheduled);
    } else {
      clearTimeout(scheduled);
    }
    scheduled = null;
    scheduledBy = '';
  };

  const emitEntries = (entries) => {
    if (entries.length > 0 && typeof onFlush === 'function') onFlush(entries);
  };

  const flushNow = () => {
    cancelScheduled();
    if (pending.size === 0) return;
    const entries = [];
    for (const entry of pending.values()) {
      let text = '';
      let event = entry.chunks[0]?.event;
      let lastIndex;
      while (entry.chunks.length > 0) {
        const chunk = entry.chunks.shift();
        event = chunk.event;
        text += chunk.text.slice(chunk.offset);
        lastIndex = chunk.index;
      }
      if (text) entries.push({ kind: entry.kind, event: combinedEvent(event, text, lastIndex) });
    }
    pending.clear();
    emitEntries(entries);
  };

  const flushFrame = () => {
    scheduled = null;
    scheduledBy = '';
    if (pending.size === 0) return;
    const entries = [];
    for (const [key, entry] of pending.entries()) {
      const budget = entry.kind === 'tool' ? Number.MAX_SAFE_INTEGER : targetCharsPerFrame;
      const chunk = takeEntryText(entry, budget);
      if (chunk.text) {
        entries.push({
          kind: entry.kind,
          event: combinedEvent(chunk.event, chunk.text, chunk.completeEventIndex)
        });
      }
      if (!entryHasPendingText(entry)) pending.delete(key);
    }
    emitEntries(entries);
    if (pending.size > 0) schedule();
  };

  const schedule = () => {
    if (scheduled !== null) return;
    if (canUseAnimationFrame()) {
      scheduledBy = 'raf';
      scheduled = window.requestAnimationFrame(flushFrame);
      return;
    }
    scheduledBy = 'timeout';
    scheduled = setTimeout(flushFrame, fallbackIntervalMs);
  };

  return {
    enqueue(kind, event) {
      const delta = streamDeltaText(event);
      if (!delta) return;
      const key = queueKey(kind, event);
      const existing = pending.get(key);
      const entry = existing || { kind, chunks: [] };
      entry.chunks.push({
        event,
        text: delta,
        offset: 0,
        index: streamEventIndex(event)
      });
      pending.set(key, entry);
      schedule();
    },
    flushNow,
    drainBefore(callback) {
      flushNow();
      if (typeof callback === 'function') callback();
    },
    reset() {
      cancelScheduled();
      pending.clear();
    },
    dispose() {
      cancelScheduled();
      pending.clear();
    }
  };
}
