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
  const renderedItems = Array.isArray(message?.items) ? message.items : [];
  const rawItems = Array.isArray(message?.data) ? message.data : [];
  if (!rawItems.length) return renderedItems;
  const renderedByIndex = new Map(
    renderedItems
      .filter((item) => item?.index !== undefined)
      .map((item) => [Number(item.index), item])
  );
  return rawItems
    .map((item, index) => normalizeChatMessageItem(item, index, renderedByIndex))
    .filter(Boolean);
}

function normalizeChatMessageItem(item, index, renderedByIndex) {
  if (!item || typeof item !== 'object') return null;
  const rendered = renderedByIndex.get(index);
  const payload = item.payload && typeof item.payload === 'object' ? item.payload : {};
  if (item.type === 'reasoning') {
    return {
      type: 'reasoning',
      index,
      text: payload.codex_summary || payload.text || '',
      summary: payload.codex_summary || ''
    };
  }
  if (item.type === 'context') {
    return rendered?.type === 'text'
      ? rendered
      : {
          type: 'text',
          index,
          text: payload.text || '',
          text_with_attachment_markers: payload.text || ''
        };
  }
  if (item.type === 'selection_reference') {
    return rendered?.type === 'selection_reference'
      ? rendered
      : {
          type: 'selection_reference',
          index,
          selection: payload
        };
  }
  if (item.type === 'file') {
    return rendered?.type === 'file' ? rendered : { type: 'file', index, file: payload };
  }
  if (item.type === 'tool_call') {
    return {
      type: 'tool_call',
      index,
      tool_call_id: payload.tool_call_id || '',
      tool_name: payload.tool_name || 'tool',
      arguments: payload.arguments?.text || payload.arguments || ''
    };
  }
  if (item.type === 'tool_result') {
    const result = payload.result || {};
    return {
      type: 'tool_result',
      index,
      tool_call_id: payload.tool_call_id || '',
      tool_name: payload.tool_name || 'tool',
      context: rendered?.context || result.context?.text || null,
      context_with_attachment_markers: rendered?.context_with_attachment_markers || result.context?.text || null,
      structured: rendered?.structured || result.structured || null,
      file_attachment_indices: rendered?.file_attachment_indices || (
        rendered?.file_attachment_index !== undefined ? [rendered.file_attachment_index] : []
      )
    };
  }
  return rendered || null;
}

export function hasToolItems(message) {
  return messageItems(message).some((item) => item?.type === 'tool_call' || item?.type === 'tool_result');
}

export function isExecutionMessage(message) {
  return hasToolItems(message) || parseToolTextBlocks(messageText(message)).length > 0;
}

export function isFinalAssistantMessage(message) {
  if (message?._streaming) return false;
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
          ? (item.structured || item.context_with_attachment_markers || item.context || '')
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
    const segments = [];
    let pendingNotes = [];
    let currentSegment = null;
    const flushSegment = () => {
      if (!currentSegment) return;
      if (currentSegment.notes.length || currentSegment.cards.length) {
        segments.push(currentSegment);
      }
      currentSegment = null;
    };
    const addNote = (kind, text) => {
      const value = String(text || '').trim();
      if (!value) return;
      if (currentSegment?.cards.length) flushSegment();
      pendingNotes.push({ kind, text: value });
    };
    const addCard = (item) => {
      if (!currentSegment) {
        currentSegment = { notes: pendingNotes, cards: [] };
        pendingNotes = [];
      }
      currentSegment.cards.push({
        id: item.tool_call_id || '',
        kind: item.type === 'tool_result' ? 'result' : 'call',
        name: item.tool_name || 'tool',
        payload: item.type === 'tool_result'
          ? (item.structured || item.context_with_attachment_markers || item.context || '')
          : (item.arguments || ''),
        usage
      });
    };
    items.forEach((item) => {
      if (item?.type === 'reasoning') {
        addNote('reasoning', item.text || item.summary || '');
      } else if (item?.type === 'text') {
        addNote('text', item.text_with_attachment_markers || item.text || item.content || '');
      } else if (item?.type === 'tool_call' || item?.type === 'tool_result') {
        addCard(item);
      }
    });
    flushSegment();
    if (pendingNotes.length) segments.push({ notes: pendingNotes, cards: [] });
    const toolCards = segments.flatMap((segment) => segment.cards);
    if (!toolCards.length) {
      return { textMessage: message, toolCards: [], segments };
    }
    const nonToolItems = items.filter((item) => !['tool_call', 'tool_result', 'reasoning', 'text'].includes(item?.type));
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
    return { textMessage, toolCards, segments };
  }
  const text = messageText(message);
  const toolCards = parseToolTextBlocks(text).map((card) => ({ ...card, usage }));
  if (!toolCards.length) {
    return { textMessage: message, toolCards: [], segments: [] };
  }
  const cleanedText = stripToolTextBlocks(text);
  const textMessage = cleanedText
    ? { ...message, text_with_attachment_markers: cleanedText, text: cleanedText, preview: cleanedText, content: cleanedText }
    : null;
  return { textMessage, toolCards, segments: [{ notes: [], cards: toolCards }] };
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

export function messageOrderFromId(value) {
  if (value === undefined || value === null || value === '') return undefined;
  const numeric = Number(value);
  if (Number.isFinite(numeric)) return numeric;
  const match = String(value).match(/^msg_(\d+)(?:_|$)/);
  if (!match) return undefined;
  const order = Number(match[1]);
  return Number.isFinite(order) ? order : undefined;
}

export function messageIndex(message) {
  const index = Number(message?.index);
  if (Number.isFinite(index)) return index;
  return messageOrderFromId(message?.id ?? message?.message_id) ?? Number.MAX_SAFE_INTEGER;
}

export function lastMessageId(messages) {
  return messages.length > 0 ? String(messages[messages.length - 1]?.id ?? '') : '';
}

export function lastServerMessageIndex(messages) {
  const last = [...messages].reverse().find((message) => {
    const index = messageIndex(message);
    return !message?._optimistic && Number.isFinite(index) && index !== Number.MAX_SAFE_INTEGER;
  });
  return last ? messageIndex(last) : undefined;
}

export function lastServerMessageId(messages) {
  const last = [...messages].reverse().find((message) => !message?._optimistic && String(message?.id ?? '').trim());
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
  const finalizedAssistantIndexes = new Set(
    incoming
      .filter((message) => String(message?.role || '').toLowerCase() === 'assistant' && !message?._streaming)
      .map(messageIndex)
      .filter((index) => Number.isFinite(index) && index !== Number.MAX_SAFE_INTEGER)
  );
  const currentWithoutEchoedOptimistic = current.filter((message) => {
    if (message?._streaming && finalizedAssistantIndexes.has(messageIndex(message))) return false;
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

export function websocketUrl(baseUrl, token, conversationId, foregroundSessionId = 'main') {
  const url = new URL(`/api/conversations/${encodeURIComponent(conversationId)}/foreground_sessions/${encodeURIComponent(foregroundSessionId || 'main')}/ws`, baseUrl);
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
      const payload = item.arguments || item.structured || item.context_with_attachment_markers || item.context || item.result || '';
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
    if (isExecutionMessage(message) || startsToolRound(source, index)) {
      const group = [];
      let cursor = index;
      while (cursor < source.length) {
        const current = source[cursor];
        if (cursor > index && isFinalAssistantMessage(current)) break;
        if (!isExecutionMessage(current) && !isRoundInterstitialMessage(current)) break;
        group.push(current);
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

function isRoundInterstitialMessage(message) {
  const role = String(message?.role || '').toLowerCase();
  if (role === 'user') return true;
  return role === 'assistant' && !isFinalAssistantMessage(message);
}

function startsToolRound(source, index) {
  const message = source[index];
  if (String(message?.role || '').toLowerCase() !== 'assistant' || isFinalAssistantMessage(message)) return false;
  for (let cursor = index + 1; cursor < source.length; cursor += 1) {
    const current = source[cursor];
    if (isFinalAssistantMessage(current)) return false;
    if (isExecutionMessage(current)) return true;
    if (!isRoundInterstitialMessage(current)) return false;
  }
  return false;
}
