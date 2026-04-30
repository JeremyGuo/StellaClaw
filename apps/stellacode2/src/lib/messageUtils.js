import { messageText } from './fileUtils';

export function markerIndexes(value) {
  const indexes = new Set();
  String(value || '').replace(/\[\[attachment:(\d+)]]/g, (_, index) => {
    indexes.add(Number(index));
    return '';
  });
  return indexes;
}

export function messageKey(message, index) {
  return message?.id || message?.message_id || `${message?.role || 'message'}-${message?.message_time || index}`;
}

export function isAuxiliaryUserMessage(message) {
  if (String(message?.role || '').toLowerCase() !== 'user') return false;
  const text = messageText(message).trim();
  return /^\[(Incoming User Metadata|Runtime Prompt Updates|Runtime Skill Updates|System Context|Developer Context|Tool Context)]/i.test(text);
}

export function messageItems(message) {
  return Array.isArray(message?.items) ? message.items : [];
}

export function hasToolItems(message) {
  return messageItems(message).some((item) => item?.type === 'tool_call' || item?.type === 'tool_result');
}

export function isExecutionMessage(message) {
  return hasToolItems(message) || parseToolTextBlocks(messageText(message)).length > 0;
}

export function isFinalAssistantMessage(message) {
  if (String(message?.role || '').toLowerCase() !== 'assistant' || isExecutionMessage(message)) return false;
  return Boolean(messageText(message).trim() || messageItems(message).some((item) => item?.type === 'text' && String(item.text || '').trim()));
}

export function shouldTypewriterMessage(message) {
  return isFinalAssistantMessage(message) && !message?._optimistic && !message?.pending && Boolean(messageText(message).trim());
}

export function parseToolTextBlocks(text) {
  const value = String(text || '');
  const blocks = [];
  const pattern = /\[tool_(call|result)\s+([^\]\n]+)\]\s*([\s\S]*?)(?=\n\[tool_(?:call|result)\s+|$)/g;
  let match;
  while ((match = pattern.exec(value)) !== null) {
    blocks.push({
      kind: match[1] === 'result' ? 'result' : 'call',
      name: match[2].trim(),
      payload: match[3].trim()
    });
  }
  return blocks;
}

export function stripToolTextBlocks(text) {
  return String(text || '')
    .replace(/\[tool_(call|result)\s+([^\]\n]+)\]\s*([\s\S]*?)(?=\n\[tool_(?:call|result)\s+|$)/g, '')
    .trim();
}

export function toolCardsForMessage(message) {
  if (hasToolItems(message)) {
    return messageItems(message)
      .filter((item) => item?.type === 'tool_call' || item?.type === 'tool_result')
      .map((item) => ({
        id: item.tool_call_id || '',
        kind: item.type === 'tool_result' ? 'result' : 'call',
        name: item.tool_name || 'tool',
        payload: item.type === 'tool_result'
          ? (item.context_with_attachment_markers || item.context || '')
          : (item.arguments || '')
      }));
  }
  return parseToolTextBlocks(messageText(message));
}

export function firstToolNameForMessage(message) {
  const items = messageItems(message);
  const toolItem = items.find((item) => item?.type === 'tool_call' || item?.type === 'tool_result');
  if (toolItem) return toolItem.tool_name || 'tool';
  return parseToolTextBlocks(messageText(message))[0]?.name || 'tool';
}

export function splitMessageForDisplay(message) {
  const usage = tokenUsage(message);
  const items = messageItems(message);
  if (items.length > 0) {
    const toolCards = items
      .filter((item) => item?.type === 'tool_call' || item?.type === 'tool_result')
      .map((item) => ({
        id: item.tool_call_id || '',
        kind: item.type === 'tool_result' ? 'result' : 'call',
        name: item.tool_name || 'tool',
        payload: item.type === 'tool_result'
          ? (item.context_with_attachment_markers || item.context || '')
          : (item.arguments || ''),
        usage
      }));
    if (!toolCards.length) {
      return { textMessage: message, toolCards: [] };
    }
    const nonToolItems = items.filter((item) => item?.type !== 'tool_call' && item?.type !== 'tool_result');
    const textMessage = nonToolItems.length > 0
      ? {
          ...message,
          items: nonToolItems,
          text: '',
          text_with_attachment_markers: '',
          preview: '',
          content: ''
        }
      : null;
    return { textMessage, toolCards };
  }
  const text = messageText(message);
  const toolCards = parseToolTextBlocks(text).map((card) => ({ ...card, usage }));
  if (!toolCards.length) {
    return { textMessage: message, toolCards: [] };
  }
  const cleanedText = stripToolTextBlocks(text);
  const textMessage = cleanedText
    ? { ...message, text_with_attachment_markers: cleanedText, text: cleanedText, preview: cleanedText, content: cleanedText }
    : null;
  return { textMessage, toolCards };
}

export function tokenUsage(message) {
  const usage = message?.token_usage || message?.usage || message?.response_usage || {};
  const input = Number(usage.input || usage.input_tokens || usage.prompt_tokens || 0);
  const output = Number(usage.output || usage.output_tokens || usage.completion_tokens || 0);
  const cacheRead = Number(usage.cache_read || usage.cache_read_tokens || 0);
  const cacheWrite = Number(usage.cache_write || usage.cache_write_tokens || 0);
  const total = Number(usage.total || usage.total_tokens || 0) || input + output + cacheRead + cacheWrite;
  const cost = usage.cost_usd || usage.cost || {};
  const costTotal = Number(cost.total || 0)
    || Number(cost.cache_read || 0)
    + Number(cost.cache_write || 0)
    + Number(cost.uncache_input || cost.input || 0)
    + Number(cost.output || 0);
  return { input, output, cacheRead, cacheWrite, total, cost: costTotal };
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

export function stableSignature(value) {
  return JSON.stringify(value ?? null);
}

export function messageIndex(message) {
  const index = Number(message?.index ?? message?.id);
  return Number.isFinite(index) ? index : Number.MAX_SAFE_INTEGER;
}

export function lastMessageId(messages) {
  return messages.length > 0 ? String(messages[messages.length - 1]?.id ?? '') : '';
}

export function lastServerMessageId(messages) {
  const last = [...messages].reverse().find((message) => !message?._optimistic && Number.isFinite(Number(message?.id)));
  return last ? String(last.id) : '';
}

export function firstMessageId(messages) {
  return messages.length > 0 ? String(messages[0]?.id ?? '') : '';
}

export function hasOlderMessages(messages) {
  return messages.length > 0 && messageIndex(messages[0]) > 0;
}

export function mergeMessages(current, incoming) {
  if (!Array.isArray(incoming) || incoming.length === 0) return current;
  const serverEchoes = incoming.filter((message) => String(message?.role || '').toLowerCase() === 'user');
  const currentWithoutEchoedOptimistic = current.filter((message) => {
    if (!message?._optimistic) return true;
    const text = messageText(message).trim();
    return !serverEchoes.some((incomingMessage) => messageText(incomingMessage).trim() === text);
  });
  const byId = new Map(currentWithoutEchoedOptimistic.map((message) => [String(message.id ?? messageKey(message, 0)), message]));
  let changed = currentWithoutEchoedOptimistic.length !== current.length;
  for (const message of incoming) {
    const id = String(message.id ?? messageKey(message, byId.size));
    const existing = byId.get(id);
    if (!existing || stableSignature(existing) !== stableSignature(message)) {
      byId.set(id, message);
      changed = true;
    }
  }
  if (!changed) return current;
  return Array.from(byId.values()).sort((left, right) => messageIndex(left) - messageIndex(right));
}

export function websocketUrl(baseUrl, token, conversationId) {
  const url = new URL(`/api/conversations/${encodeURIComponent(conversationId)}/foreground/ws`, baseUrl);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('token', token || '');
  return url.toString();
}

export function activityFromMessages(messages) {
  const last = [...messages].reverse().find((message) => messageItems(message).length > 0);
  const item = [...messageItems(last)].reverse().find((entry) => entry?.type === 'tool_call' || entry?.type === 'tool_result' || entry?.type === 'text');
  if (!item) return '';
  if (item.type === 'tool_call') return `正在调用 ${item.tool_name || '工具'}`;
  if (item.type === 'tool_result') return `${item.tool_name || '工具'} 已返回`;
  return '正在回复';
}

export function shortText(value, max = 96) {
  const text = String(value || '').replace(/\s+/g, ' ').trim();
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

export function liveActivitySignature(items) {
  return stableSignature(items.map((item) => ({
    id: item.id,
    title: item.title,
    detail: item.detail,
    state: item.state
  })));
}

export function liveActivitiesFromMessages(messages) {
  const result = [];
  for (const message of messages || []) {
    messageItems(message).forEach((item, index) => {
      if (item?.type !== 'tool_call' && item?.type !== 'tool_result') return;
      const name = item.tool_name || 'tool';
      const id = item.tool_call_id || `${messageKey(message, index)}-${index}`;
      const payload = item.arguments || item.context_with_attachment_markers || item.context || item.result || '';
      result.push({
        id: `${item.type}-${id}`,
        title: item.type === 'tool_result' ? '工具已返回' : '调用工具',
        detail: `${name}${payload ? ` · ${shortText(typeof payload === 'string' ? payload : JSON.stringify(payload), 80)}` : ''}`,
        state: item.type === 'tool_result' ? 'done' : 'running'
      });
    });
  }
  return result;
}

export function attachAuxiliaryMessages(messages) {
  const result = [];
  let aux = [];
  messages.forEach((message, index) => {
    if (isAuxiliaryUserMessage(message)) {
      aux.push(message);
      return;
    }
    if (String(message?.role || '').toLowerCase() === 'user' && aux.length) {
      result.push({ ...message, _auxiliary: aux });
      aux = [];
      return;
    }
    result.push(message);
  });
  result.push(...aux);
  return result;
}

export function displayMessages(messages) {
  const source = attachAuxiliaryMessages(messages);
  const result = [];
  let forceSeparateNext = false;
  for (let index = 0; index < source.length; index += 1) {
    const message = source[index];
    if (isExecutionMessage(message)) {
      const group = [];
      let cursor = index;
      while (cursor < source.length && isExecutionMessage(source[cursor])) {
        group.push(source[cursor]);
        cursor += 1;
      }
      const nextMessage = source[cursor];
      result.push({ type: 'toolGroup', id: `tools-${messageKey(group[0], index)}`, messages: group, nextMessage });
      forceSeparateNext = Boolean(isFinalAssistantMessage(nextMessage));
      index = cursor - 1;
      continue;
    }
    result.push(forceSeparateNext ? { ...message, _forceSeparate: true } : message);
    forceSeparateNext = false;
  }
  return result;
}
