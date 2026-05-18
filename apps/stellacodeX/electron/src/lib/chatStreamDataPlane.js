import { messageIndex, messageOrderFromId } from './messageUtils';

export function normalizedStreamEvent(payload) {
  return payload?.event || payload?.session_event || payload?.stream_event || payload;
}

export function streamEventType(event) {
  const raw = String(event?.type || event?.event_type || event?.kind || '').toLowerCase();
  return raw.startsWith('chat.') ? raw.slice('chat.'.length) : raw;
}

export function streamMessageId(event) {
  return String(
    event?.message_id
    || event?.messageId
    || event?.stream_id
    || event?.streamId
    || event?.item_id
    || event?.itemId
    || event?.turn_id
    || event?.turnId
    || ''
  ).trim();
}

export function streamActivityBaseId(event) {
  return streamMessageId(event) || 'current';
}

export function streamItemId(event) {
  return String(event?.call_id || event?.callId || event?.item_id || event?.itemId || streamActivityBaseId(event)).trim();
}

export function streamDeltaText(event) {
  return String(event?.delta ?? event?.text_delta ?? event?.textDelta ?? '');
}

export function streamEventIndex(event) {
  const index = Number(event?.in_message_index ?? event?.inMessageIndex);
  return Number.isFinite(index) && index >= 0 ? index : undefined;
}

export function streamErrorText(event) {
  return String(event?.error || event?.message || event?.error_detail || event?.errorDetail || '流式响应失败').trim();
}

export function streamMessageIndexFromEvent(event) {
  const fromMessageId = messageOrderFromId(streamMessageId(event));
  if (Number.isFinite(fromMessageId)) return fromMessageId;
  const explicit = Number(event?.message_index ?? event?.messageIndex ?? event?.index);
  if (Number.isFinite(explicit)) return explicit;
  return undefined;
}

export function createStreamBufferStore() {
  const buffers = new Map();
  return {
    append(bufferKey, delta) {
      if (!delta) return buffers.get(bufferKey) || '';
      const next = `${buffers.get(bufferKey) || ''}${delta}`;
      buffers.set(bufferKey, next);
      return next;
    },
    reset() {
      buffers.clear();
    }
  };
}

export function createStreamIndexTracker(scopeKey) {
  const nextIndices = new Map();
  const scopedMessageId = (event) => {
    const messageId = streamMessageId(event);
    return messageId ? `${scopeKey}:${messageId}` : '';
  };
  return {
    accept(event, onGap) {
      const scopedId = scopedMessageId(event);
      const index = streamEventIndex(event);
      if (!scopedId || index === undefined) return true;
      const expected = nextIndices.get(scopedId);
      if (expected === undefined) {
        nextIndices.set(scopedId, index + 1);
        return true;
      }
      if (index < expected) return false;
      if (index > expected) {
        nextIndices.set(scopedId, index + 1);
        if (typeof onGap === 'function') onGap(expected, index);
        return false;
      }
      nextIndices.set(scopedId, expected + 1);
      return true;
    },
    clearForEvent(event) {
      const scopedId = scopedMessageId(event);
      if (scopedId) nextIndices.delete(scopedId);
    },
    reset() {
      nextIndices.clear();
    }
  };
}

export function removeStreamingMessagesForTurn(current, turnId = '') {
  const scopedTurnId = String(turnId || '').trim();
  let changed = false;
  const next = current.filter((message) => {
    if (!message?._streaming || String(message?.role || '').toLowerCase() !== 'assistant') return true;
    const messageTurnId = String(message?._streamTurnId || message?.turn_id || message?.turnId || '').trim();
    const remove = scopedTurnId ? messageTurnId === scopedTurnId : true;
    if (remove) changed = true;
    return !remove;
  });
  return changed ? next : current;
}

function nextStreamMessageIndex(messages) {
  let last = undefined;
  let optimisticUsers = 0;
  for (const message of messages || []) {
    if (message?._optimistic) {
      if (String(message?.role || '').toLowerCase() === 'user') optimisticUsers += 1;
      continue;
    }
    if (message?._streaming) continue;
    const index = messageIndex(message);
    if (Number.isFinite(index) && index !== Number.MAX_SAFE_INTEGER) {
      last = last === undefined ? index : Math.max(last, index);
    }
  }
  if (last !== undefined) return last + optimisticUsers + 1;
  return optimisticUsers > 0 ? optimisticUsers : undefined;
}

function appendTextDelta(existingText, delta) {
  const previous = String(existingText || '');
  const chunk = String(delta || '');
  return `${previous}${chunk}`;
}

function findStreamingMessagePosition(current, id, turnId) {
  const messageId = String(id || '').trim();
  const streamTurnId = String(turnId || '').trim();
  return current.findIndex((message) => {
    if (!message?._streaming || String(message?.role || '').toLowerCase() !== 'assistant') return false;
    const existingId = String(message?.id ?? message?.message_id ?? '').trim();
    if (messageId && existingId === messageId) return true;
    return Boolean(streamTurnId && String(message?._streamTurnId || '') === streamTurnId);
  });
}

export function appendStreamAssistantDelta(current, event) {
  const id = streamMessageId(event);
  const delta = streamDeltaText(event);
  if (!id || !delta) return current;
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const itemId = String(event?.item_id || event?.itemId || '').trim();
  const position = findStreamingMessagePosition(current, id, turnId);
  const now = new Date().toISOString();
  const fallbackIndex = nextStreamMessageIndex(current);
  const buildMessage = (existing = {}) => {
    const nextText = appendTextDelta(existing.text || existing.preview || '', delta);
    const items = Array.isArray(existing.items) ? [...existing.items] : [];
    const textIndex = items.findIndex((item) => item?.type === 'text');
    const textItem = {
      type: 'text',
      index: textIndex >= 0 ? items[textIndex].index : items.length,
      text: nextText,
      text_with_attachment_markers: nextText
    };
    if (textIndex >= 0) {
      items[textIndex] = { ...items[textIndex], ...textItem };
    } else {
      items.push(textItem);
    }
    const eventIndex = streamMessageIndexFromEvent(event);
    const existingIndex = Number(existing.index);
    const index = Number.isFinite(eventIndex)
      ? eventIndex
      : Number.isFinite(existingIndex)
        ? existingIndex
        : fallbackIndex;
    return {
      ...existing,
      id,
      message_id: id,
      index: Number.isFinite(index) ? index : existing.index,
      role: 'assistant',
      text: nextText,
      preview: nextText,
      content: nextText,
      text_with_attachment_markers: nextText,
      items,
      attachments: existing.attachments || [],
      attachment_count: existing.attachment_count || 0,
      message_time: existing.message_time || now,
      _streamTurnId: turnId || existing._streamTurnId || '',
      _streamItemId: itemId || existing._streamItemId || '',
      _streaming: true
    };
  };
  if (position < 0) return [...current, buildMessage()];
  const next = [...current];
  next[position] = buildMessage(next[position]);
  return next;
}

export function appendStreamToolCallDelta(current, event) {
  const id = streamMessageId(event);
  const delta = streamDeltaText(event);
  if (!id || !delta) return current;
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const itemId = String(event?.item_id || event?.itemId || '').trim();
  const callId = String(event?.call_id || event?.callId || itemId).trim();
  if (!callId) return current;
  const position = findStreamingMessagePosition(current, id, turnId);
  const now = new Date().toISOString();
  const fallbackIndex = nextStreamMessageIndex(current);
  const buildMessage = (existing = {}) => {
    const items = Array.isArray(existing.items) ? [...existing.items] : [];
    const itemIndex = items.findIndex((item) => item?.type === 'tool_call' && String(item?.tool_call_id || '') === callId);
    const label = String(event?.tool_name || event?.toolName || '').trim()
      || (itemId && !/^item_|^fc_|^call_/.test(itemId) ? itemId : 'tool');
    if (itemIndex >= 0) {
      items[itemIndex] = {
        ...items[itemIndex],
        tool_name: label,
        arguments: appendTextDelta(items[itemIndex].arguments || '', delta)
      };
    } else {
      items.push({
        type: 'tool_call',
        index: items.length,
        tool_call_id: callId,
        tool_name: label,
        arguments: delta
      });
    }
    const eventIndex = streamMessageIndexFromEvent(event);
    const existingIndex = Number(existing.index);
    const index = Number.isFinite(eventIndex)
      ? eventIndex
      : Number.isFinite(existingIndex)
        ? existingIndex
        : fallbackIndex;
    return {
      ...existing,
      id,
      message_id: id,
      index: Number.isFinite(index) ? index : existing.index,
      role: 'assistant',
      text: existing.text || existing.preview || '',
      preview: existing.preview || existing.text || '',
      content: existing.content || existing.text || existing.preview || '',
      text_with_attachment_markers: existing.text_with_attachment_markers || existing.text || existing.preview || '',
      items,
      attachments: existing.attachments || [],
      attachment_count: existing.attachment_count || 0,
      message_time: existing.message_time || now,
      _streamTurnId: turnId || existing._streamTurnId || '',
      _streamItemId: itemId || existing._streamItemId || '',
      _streaming: true
    };
  };
  if (position < 0) return [...current, buildMessage()];
  const next = [...current];
  next[position] = buildMessage(next[position]);
  return next;
}

export function appendStreamReasoningSummary(current, event) {
  const id = streamMessageId(event);
  if (!id) return current;
  const delta = streamDeltaText(event);
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const itemId = String(event?.item_id || event?.itemId || '').trim();
  const summaryIndex = Number(event?.summary_index ?? event?.summaryIndex ?? 0);
  const position = findStreamingMessagePosition(current, id, turnId);
  const now = new Date().toISOString();
  const fallbackIndex = nextStreamMessageIndex(current);
  const buildMessage = (existing = {}) => {
    const items = Array.isArray(existing.items) ? [...existing.items] : [];
    const reasoningIndex = items.findIndex((item) => (
      item?.type === 'reasoning'
      && Number(item?._summaryIndex ?? item?.summary_index ?? item?.summaryIndex ?? 0) === summaryIndex
    ));
    if (reasoningIndex >= 0) {
      const text = appendTextDelta(items[reasoningIndex].text || items[reasoningIndex].summary || '', delta);
      items[reasoningIndex] = {
        ...items[reasoningIndex],
        text,
        summary: text,
        _summaryIndex: summaryIndex
      };
    } else {
      items.push({
        type: 'reasoning',
        index: items.length,
        text: delta,
        summary: delta,
        _summaryIndex: summaryIndex
      });
    }
    const eventIndex = streamMessageIndexFromEvent(event);
    const existingIndex = Number(existing.index);
    const index = Number.isFinite(eventIndex)
      ? eventIndex
      : Number.isFinite(existingIndex)
        ? existingIndex
        : fallbackIndex;
    return {
      ...existing,
      id,
      message_id: id,
      index: Number.isFinite(index) ? index : existing.index,
      role: 'assistant',
      text: existing.text || existing.preview || '',
      preview: existing.preview || existing.text || '',
      content: existing.content || existing.text || existing.preview || '',
      text_with_attachment_markers: existing.text_with_attachment_markers || existing.text || existing.preview || '',
      items,
      attachments: existing.attachments || [],
      attachment_count: existing.attachment_count || 0,
      message_time: existing.message_time || now,
      _streamTurnId: turnId || existing._streamTurnId || '',
      _streamItemId: itemId || existing._streamItemId || '',
      _streaming: true
    };
  };
  if (position < 0) return [...current, buildMessage()];
  const next = [...current];
  next[position] = buildMessage(next[position]);
  return next;
}

function liveToolResultMessage(event, existingMessages = []) {
  const toolResult = event?.tool_result || event?.toolResult || event;
  if (!toolResult || typeof toolResult !== 'object') return null;
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const toolCallId = String(toolResult.tool_call_id || toolResult.toolCallId || event?.tool_call_id || event?.toolCallId || '').trim();
  const toolName = String(toolResult.tool_name || toolResult.toolName || 'tool').trim() || 'tool';
  if (!toolCallId && !toolName) return null;
  const result = toolResult.result || {};
  const id = `live-tool-result-${turnId || 'turn'}-${toolCallId || toolName}`;
  return {
    id,
    message_id: id,
    index: nextStreamMessageIndex(existingMessages),
    role: 'assistant',
    text: '',
    preview: '',
    content: '',
    text_with_attachment_markers: '',
    items: [{
      type: 'tool_result',
      index: 0,
      tool_call_id: toolCallId,
      tool_name: toolName,
      context: result.context?.text || null,
      context_with_attachment_markers: result.context?.text || null,
      structured: result.structured || null,
      files: Array.isArray(result.files) ? result.files : []
    }],
    attachments: Array.isArray(result.files) ? result.files : [],
    attachment_count: Array.isArray(result.files) ? result.files.length : 0,
    message_time: new Date().toISOString(),
    _streamTurnId: turnId,
    _streaming: true,
    _liveToolResult: true,
    _liveToolCallId: toolCallId
  };
}

function appendToolResultToStreamingMessage(current, event) {
  const toolResult = event?.tool_result || event?.toolResult || event;
  if (!toolResult || typeof toolResult !== 'object') return null;
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const toolCallId = String(toolResult.tool_call_id || toolResult.toolCallId || event?.tool_call_id || event?.toolCallId || '').trim();
  const toolName = String(toolResult.tool_name || toolResult.toolName || 'tool').trim() || 'tool';
  if (!toolCallId && !toolName) return null;
  const position = current.findIndex((message) => (
    message?._streaming
    && String(message?.role || '').toLowerCase() === 'assistant'
    && (!turnId || String(message?._streamTurnId || '') === turnId)
  ));
  if (position < 0) return null;
  const result = toolResult.result || {};
  const next = [...current];
  const message = next[position];
  const items = Array.isArray(message.items) ? [...message.items] : [];
  const existingIndex = items.findIndex((item) => (
    item?.type === 'tool_result'
    && String(item?.tool_call_id || '') === toolCallId
  ));
  const item = {
    type: 'tool_result',
    index: existingIndex >= 0 ? items[existingIndex].index : items.length,
    tool_call_id: toolCallId,
    tool_name: toolName,
    context: result.context?.text || null,
    context_with_attachment_markers: result.context?.text || null,
    structured: result.structured || null,
    files: Array.isArray(result.files) ? result.files : []
  };
  if (existingIndex >= 0) items[existingIndex] = { ...items[existingIndex], ...item };
  else items.push(item);
  const resultFiles = existingIndex >= 0 ? [] : (Array.isArray(result.files) ? result.files : []);
  next[position] = {
    ...message,
    items,
    attachments: [
      ...(Array.isArray(message.attachments) ? message.attachments : []),
      ...resultFiles
    ],
    attachment_count: Number(message.attachment_count || 0) + resultFiles.length
  };
  return next;
}

export function appendStreamToolResultDone(current, event) {
  const merged = appendToolResultToStreamingMessage(current, event);
  if (merged) return merged;
  const message = liveToolResultMessage(event, current);
  if (!message) return current;
  const existingIndex = current.findIndex((item) => (
    item?._liveToolResult
    && String(item?._liveToolCallId || '') === String(message._liveToolCallId || '')
    && String(item?._streamTurnId || '') === String(message._streamTurnId || '')
  ));
  if (existingIndex < 0) return [...current, message];
  const next = [...current];
  next[existingIndex] = {
    ...next[existingIndex],
    ...message,
    index: next[existingIndex].index
  };
  return next;
}

export function markQueuedUserMessage(current, clientMessageId) {
  const id = String(clientMessageId || '').trim();
  if (!id) return current;
  let changed = false;
  const next = current.map((message) => {
    if (String(message?.id ?? message?.message_id ?? '') !== id) return message;
    changed = true;
    return {
      ...message,
      pending: false,
      queued: true
    };
  });
  return changed ? next : current;
}

export function applyStreamErrorToMessages(current, event) {
  const id = streamMessageId(event);
  const error = streamErrorText(event);
  if (!id) {
    let changed = false;
    const next = current.filter((message) => {
      const remove = message?._streaming && String(message?.role || '').toLowerCase() === 'assistant';
      if (remove) changed = true;
      return !remove;
    });
    return changed ? next : current;
  }
  const position = current.findIndex((message) => String(message?.id ?? message?.message_id ?? '') === id);
  if (position < 0) {
    const index = streamMessageIndexFromEvent(event);
    return [
      ...current,
      {
        id,
        message_id: id,
        index: Number.isFinite(index) ? index : undefined,
        role: 'assistant',
        text: '',
        preview: '',
        items: [],
        attachments: [],
        attachment_count: 0,
        message_time: new Date().toISOString(),
        error,
        _streaming: false,
        _streamFailed: true
      }
    ];
  }
  const next = [...current];
  next[position] = {
    ...next[position],
    _streaming: false,
    _streamFailed: true,
    error
  };
  return next;
}

export function streamFinalizedActivityIds(messages) {
  const ids = new Set();
  for (const message of messages || []) {
    const id = String(message?.id ?? message?.message_id ?? '').trim();
    if (id) {
      ids.add(`stream-assistant-${id}`);
      ids.add(`stream-reasoning-${id}`);
      ids.add(`stream-tool-${id}`);
    }
    const items = [
      ...(Array.isArray(message?.items) ? message.items : []),
      ...(Array.isArray(message?.data) ? message.data : [])
    ];
    for (const item of items) {
      if (item?.type !== 'tool_call' && item?.type !== 'tool_result') continue;
      const payload = item?.payload && typeof item.payload === 'object' ? item.payload : item;
      const toolId = String(payload?.tool_call_id || payload?.call_id || payload?.item_id || '').trim();
      if (toolId) ids.add(`stream-tool-${toolId}`);
    }
  }
  return ids;
}
