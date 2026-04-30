export function clamp(value, min, max) {
  return Math.min(max, Math.max(min, Number(value) || min));
}

export function formatModel(conversation, status) {
  const modelSelectionPending = conversation?.model_selection_pending ?? status?.model_selection_pending;
  if (modelSelectionPending) {
    return 'pending';
  }
  return conversation?.model || status?.model || '';
}

export function modelAlias(model) {
  if (typeof model === 'string') return model;
  return model?.alias || model?.name || model?.id || '';
}

export function modelDisplayName(model) {
  if (!model || typeof model === 'string') return '';
  return [model.model_name || model.display_name || '', model.provider_type || ''].filter(Boolean).join(' · ');
}

export function formatBytes(value) {
  const size = Number(value || 0);
  if (!size) return '';
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(size < 10 * 1024 ? 1 : 0)} KB`;
  return `${(size / 1024 / 1024).toFixed(1)} MB`;
}

export function formatTokens(value) {
  const total = Number(value || 0);
  if (total >= 1000) return `${Math.round(total / 1000)}K tokens`;
  return `${total} tokens`;
}

export function formatCompactNumber(value) {
  const number = Number(value || 0);
  if (number >= 1_000_000) return `${(number / 1_000_000).toFixed(1)}M`;
  if (number >= 10_000) return `${Math.round(number / 1000)}K`;
  return number.toLocaleString();
}

export function formatCost(value) {
  return `$${Number(value || 0).toFixed(3)}`;
}

export function emptyUsageTotals() {
  return {
    cacheRead: 0,
    cacheWrite: 0,
    input: 0,
    output: 0,
    cost: 0,
    totalTokens: 0,
    cacheHit: 0
  };
}

export function normalizeUsageTotals(value) {
  const usage = {
    ...emptyUsageTotals(),
    ...(value || {})
  };
  usage.totalTokens = Number(usage.totalTokens || 0) || Number(usage.cacheRead || 0) + Number(usage.cacheWrite || 0) + Number(usage.input || 0) + Number(usage.output || 0);
  usage.cacheHit = usage.totalTokens > 0
    ? Number(usage.cacheRead || 0) / usage.totalTokens
    : 0;
  return usage;
}

export function addUsageTotals(left, right) {
  const a = normalizeUsageTotals(left);
  const b = normalizeUsageTotals(right);
  return normalizeUsageTotals({
    cacheRead: a.cacheRead + b.cacheRead,
    cacheWrite: a.cacheWrite + b.cacheWrite,
    input: a.input + b.input,
    output: a.output + b.output,
    cost: a.cost + b.cost
  });
}

export function statusUsageTotals(status, delta) {
  let totals = emptyUsageTotals();
  for (const bucket of Object.values(status?.usage || {})) {
    const cost = bucket?.cost || {};
    totals = addUsageTotals(totals, {
      cacheRead: Number(bucket?.cache_read || 0),
      cacheWrite: Number(bucket?.cache_write || 0),
      input: Number(bucket?.uncache_input || bucket?.input || 0),
      output: Number(bucket?.output || 0),
      cost:
        Number(cost.cache_read || 0)
        + Number(cost.cache_write || 0)
        + Number(cost.uncache_input || cost.input || 0)
        + Number(cost.output || 0)
    });
  }
  return addUsageTotals(totals, delta);
}

export function usageDeltaFromMessages(scopeKey, messages, seenByScope) {
  let seen = seenByScope.get(scopeKey);
  if (!seen) {
    seen = new Set();
    seenByScope.set(scopeKey, seen);
  }
  let totals = emptyUsageTotals();
  for (const message of messages || []) {
    const id = String(message?.id ?? message?.index ?? '');
    if (!id || seen.has(id) || !message?.has_token_usage && !message?.token_usage && !message?.usage && !message?.response_usage) continue;
    seen.add(id);
    const usage = tokenUsage(message);
    totals = addUsageTotals(totals, {
      cacheRead: usage.cacheRead,
      cacheWrite: usage.cacheWrite,
      input: usage.input,
      output: usage.output,
      cost: usage.cost
    });
  }
  return totals;
}
