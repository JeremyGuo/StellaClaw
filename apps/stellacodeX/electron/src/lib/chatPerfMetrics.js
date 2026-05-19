const metrics = new Map();
const events = [];
const MAX_EVENTS = 80;
const MAX_META_TEXT = 220;

function now() {
  return typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now();
}

function wallTime() {
  return new Date().toLocaleTimeString(undefined, { hour12: false });
}

export function measureChatPerf(name, fn, meta) {
  const startedAt = now();
  try {
    return fn();
  } finally {
    recordChatPerf(name, now() - startedAt, meta);
  }
}

export function recordChatPerf(name, durationMs = 0, meta) {
  const key = String(name || 'unknown');
  const duration = Number(durationMs || 0);
  const current = metrics.get(key) || {
    name: key,
    count: 0,
    totalMs: 0,
    maxMs: 0,
    lastMs: 0,
    lastAt: 0,
    lastMeta: null
  };
  current.count += 1;
  current.totalMs += duration;
  current.maxMs = Math.max(current.maxMs, duration);
  current.lastMs = duration;
  current.lastAt = Date.now();
  current.lastMeta = compactMeta(meta);
  metrics.set(key, current);
  if (duration >= eventThresholdMs(key)) {
    events.push({
      time: wallTime(),
      name: key,
      durationMs: roundMs(duration),
      meta: current.lastMeta
    });
    if (events.length > MAX_EVENTS) events.splice(0, events.length - MAX_EVENTS);
  }
}

export function countChatPerf(name, count = 1, meta) {
  recordChatPerf(name, 0, { ...compactMeta(meta), count });
}

export function snapshotChatPerf() {
  const rows = Array.from(metrics.values())
    .map((metric) => ({
      ...metric,
      avgMs: metric.count ? metric.totalMs / metric.count : 0,
      totalMs: roundMs(metric.totalMs),
      maxMs: roundMs(metric.maxMs),
      lastMs: roundMs(metric.lastMs),
      avgMsRounded: roundMs(metric.count ? metric.totalMs / metric.count : 0)
    }))
    .sort((a, b) => b.totalMs - a.totalMs || b.maxMs - a.maxMs || a.name.localeCompare(b.name));
  return {
    capturedAt: wallTime(),
    rows,
    events: events.slice().reverse()
  };
}

export function clearChatPerf() {
  metrics.clear();
  events.length = 0;
}

export function startChatFrameProbe(enabled) {
  if (!enabled || typeof window === 'undefined') return () => {};
  let frame = 0;
  let last = now();
  const tick = () => {
    const current = now();
    const delta = current - last;
    last = current;
    if (delta > 24) {
      recordChatPerf('browser.frame_gap', delta, { fps: delta > 0 ? Math.round(1000 / delta) : 0 });
    }
    frame = window.requestAnimationFrame(tick);
  };
  frame = window.requestAnimationFrame(tick);
  return () => {
    if (frame) window.cancelAnimationFrame(frame);
  };
}

function eventThresholdMs(name) {
  if (name === 'browser.frame_gap') return 34;
  if (name === 'chat.workspace.render_commit') return 16;
  if (name.includes('commit')) return 12;
  return 8;
}

function compactMeta(meta) {
  if (!meta || typeof meta !== 'object') return meta ?? null;
  const result = {};
  Object.entries(meta).forEach(([key, value]) => {
    if (value === undefined || typeof value === 'function') return;
    if (typeof value === 'string') {
      result[key] = value.length > MAX_META_TEXT ? `${value.slice(0, MAX_META_TEXT)}...` : value;
    } else if (typeof value === 'number' || typeof value === 'boolean' || value === null) {
      result[key] = value;
    } else if (Array.isArray(value)) {
      result[key] = value.slice(0, 8).map((item) => (
        typeof item === 'string' && item.length > 80 ? `${item.slice(0, 80)}...` : item
      ));
    } else {
      try {
        const text = JSON.stringify(value);
        result[key] = text.length > MAX_META_TEXT ? `${text.slice(0, MAX_META_TEXT)}...` : value;
      } catch {
        result[key] = String(value);
      }
    }
  });
  return result;
}

function roundMs(value) {
  return Math.round(Number(value || 0) * 10) / 10;
}
