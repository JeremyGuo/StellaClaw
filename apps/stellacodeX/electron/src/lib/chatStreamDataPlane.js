import { messageIndex, messageOrderFromId, shortText } from './messageUtils';

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
    || event?.next_message_id
    || event?.nextMessageId
    || event?.stream_id
    || event?.streamId
    || ''
  ).trim();
}

export function streamActivityBaseId(event) {
  return String(
    streamMessageId(event)
    || event?.item_id
    || event?.itemId
    || event?.turn_id
    || event?.turnId
    || 'current'
  ).trim();
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
  const invalidStreams = new Set();
  const scopedMessageId = (event) => {
    const messageId = streamMessageId(event);
    return messageId ? `${scopeKey}:${messageId}` : '';
  };
  return {
    accept(event, onGap, expectedFromState) {
      const scopedId = scopedMessageId(event);
      const index = streamEventIndex(event);
      if (!scopedId || index === undefined) return true;
      if (invalidStreams.has(scopedId)) return false;
      const expected = nextIndices.get(scopedId);
      if (expected === undefined) {
        const stateExpected = Number(expectedFromState);
        if (Number.isFinite(stateExpected)) {
          if (index < stateExpected) return false;
          if (index > stateExpected) {
            invalidStreams.add(scopedId);
            if (typeof onGap === 'function') onGap(stateExpected, index);
            return false;
          }
          nextIndices.set(scopedId, index + 1);
          return true;
        }
        if (index > 0) {
          invalidStreams.add(scopedId);
          if (typeof onGap === 'function') onGap(0, index);
          return false;
        }
        nextIndices.set(scopedId, index + 1);
        return true;
      }
      if (index < expected) return false;
      if (index > expected) {
        invalidStreams.add(scopedId);
        if (typeof onGap === 'function') onGap(expected, index);
        return false;
      }
      nextIndices.set(scopedId, expected + 1);
      return true;
    },
    clearForEvent(event) {
      const scopedId = scopedMessageId(event);
      if (scopedId) {
        nextIndices.delete(scopedId);
        invalidStreams.delete(scopedId);
      }
    },
    reset() {
      nextIndices.clear();
      invalidStreams.clear();
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

function findStreamingMessagePositions(current, id, turnId) {
  const messageId = String(id || '').trim();
  const streamTurnId = String(turnId || '').trim();
  const positions = [];
  current.forEach((message, index) => {
    if (!message?._streaming || String(message?.role || '').toLowerCase() !== 'assistant') return false;
    const existingId = String(message?.id ?? message?.message_id ?? '').trim();
    if (messageId && existingId === messageId) {
      positions.push(index);
      return;
    }
    if (!messageId && streamTurnId && String(message?._streamTurnId || '') === streamTurnId) {
      positions.push(index);
    }
  });
  return positions;
}

function streamingMessageId(message) {
  return String(message?.id ?? message?.message_id ?? '').trim();
}

function sameStreamingMessagePositions(current, position) {
  const message = current[position];
  const messageId = streamingMessageId(message);
  if (!messageId) return [position];
  return current
    .map((currentMessage, index) => (
      currentMessage?._streaming
      && String(currentMessage?.role || '').toLowerCase() === 'assistant'
      && streamingMessageId(currentMessage) === messageId
        ? index
        : -1
    ))
    .filter((index) => index >= 0);
}

function toolCallIdentityValues(value) {
  if (!value || typeof value !== 'object') return [];
  return [
    value.tool_call_id,
    value.toolCallId,
    value.call_id,
    value.callId,
    value.item_id,
    value.itemId,
    value._item_id,
    value._streamItemId
  ]
    .map((item) => String(item || '').trim())
    .filter(Boolean);
}

function toolCallMatches(item, targetIds) {
  if (!targetIds.size) return false;
  const itemIds = toolCallIdentityValues(item);
  return itemIds.some((id) => targetIds.has(id));
}

function findStreamingToolResultPosition(current, turnId, toolCallId, toolName) {
  const streamTurnId = String(turnId || '').trim();
  const targetIds = new Set(toolCallIdentityValues({
    tool_call_id: toolCallId,
    call_id: toolCallId,
    item_id: toolCallId
  }));
  const toolLabel = String(toolName || '').trim();
  const candidates = [];
  current.forEach((message, index) => {
    if (!message?._streaming || String(message?.role || '').toLowerCase() !== 'assistant') return;
    if (streamTurnId && String(message?._streamTurnId || '') !== streamTurnId) return;
    candidates.push(index);
  });
  const exact = candidates.find((index) => {
    const items = Array.isArray(current[index]?.items) ? current[index].items : [];
    return items.some((item) => item?.type === 'tool_call' && toolCallMatches(item, targetIds));
  });
  if (exact !== undefined) return exact;
  let nameMatch;
  for (let i = candidates.length - 1; i >= 0; i -= 1) {
    const index = candidates[i];
    const items = Array.isArray(current[index]?.items) ? current[index].items : [];
    if (toolLabel && items.some((item) => (
      item?.type === 'tool_call'
      && String(item?.tool_name || item?.toolName || '').trim() === toolLabel
    ))) {
      nameMatch = index;
      break;
    }
  }
  if (nameMatch !== undefined) return nameMatch;
  return candidates.length ? candidates[candidates.length - 1] : -1;
}

function upsertStreamingMessage(current, id, turnId, buildMessage) {
  const positions = findStreamingMessagePositions(current, id, turnId);
  if (!positions.length) return [...current, buildMessage()];
  const selectedPositions = new Set(positions);
  const insertAt = positions[0];
  const base = current[positions[positions.length - 1]] || {};
  const nextMessage = buildMessage(base);
  const next = [];
  current.forEach((message, index) => {
    if (index === insertAt) {
      next.push(nextMessage);
    } else if (!selectedPositions.has(index)) {
      next.push(message);
    }
  });
  return next;
}

export function appendStreamAssistantDelta(current, event, fullText) {
  const id = streamMessageId(event);
  const delta = streamDeltaText(event);
  if (!id || !delta) return current;
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const itemId = String(event?.item_id || event?.itemId || '').trim();
  const now = new Date().toISOString();
  const fallbackIndex = nextStreamMessageIndex(current);
  const buildMessage = (existing = {}) => {
    const nextText = String(fullText || '') || appendTextDelta(existing.text || existing.content || existing.text_with_attachment_markers || existing.preview || '', delta);
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
      _lastStreamEventIndex: streamEventIndex(event) ?? existing._lastStreamEventIndex,
      _streamTurnId: turnId || existing._streamTurnId || '',
      _streamItemId: itemId || existing._streamItemId || '',
      _streaming: true
    };
  };
  return upsertStreamingMessage(current, id, turnId, buildMessage);
}

export function appendStreamToolCallDelta(current, event) {
  const id = streamMessageId(event);
  const delta = streamDeltaText(event);
  if (!id || !delta) return current;
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const itemId = String(event?.item_id || event?.itemId || '').trim();
  const callId = String(event?.call_id || event?.callId || itemId).trim();
  if (!callId) return current;
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
      _lastStreamEventIndex: streamEventIndex(event) ?? existing._lastStreamEventIndex,
      _streamTurnId: turnId || existing._streamTurnId || '',
      _streamItemId: itemId || existing._streamItemId || '',
      _streaming: true
    };
  };
  return upsertStreamingMessage(current, id, turnId, buildMessage);
}

export function appendStreamReasoningSummary(current, event) {
  const id = streamMessageId(event);
  if (!id) return current;
  const delta = streamDeltaText(event);
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const itemId = String(event?.item_id || event?.itemId || '').trim();
  const summaryIndex = Number(event?.summary_index ?? event?.summaryIndex ?? 0);
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
      _lastStreamEventIndex: streamEventIndex(event) ?? existing._lastStreamEventIndex,
      _streamTurnId: turnId || existing._streamTurnId || '',
      _streamItemId: itemId || existing._streamItemId || '',
      _streaming: true
    };
  };
  return upsertStreamingMessage(current, id, turnId, buildMessage);
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
    _lastStreamEventIndex: streamEventIndex(event),
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
  const position = findStreamingToolResultPosition(current, turnId, toolCallId, toolName);
  if (position < 0) return null;
  const result = toolResult.result || {};
  const positions = sameStreamingMessagePositions(current, position);
  const message = current[position] || {};
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
  const updatedMessage = {
    ...message,
    items,
    attachments: [
      ...(Array.isArray(message.attachments) ? message.attachments : []),
      ...resultFiles
    ],
    attachment_count: Number(message.attachment_count || 0) + resultFiles.length
  };
  const resultEventIndex = streamEventIndex(event);
  if (resultEventIndex !== undefined) {
    updatedMessage._lastStreamEventIndex = resultEventIndex;
  }
  const selectedPositions = new Set(positions);
  const next = [];
  current.forEach((currentMessage, index) => {
    if (index === position) {
      next.push(updatedMessage);
    } else if (!selectedPositions.has(index)) {
      next.push(currentMessage);
    }
  });
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

export function streamTurnStartedPatch(event) {
  return {
    resetStreamState: true,
    chatState: {
      state: 'running',
      currentTurnState: event,
      activeTurnId: String(event?.turn_id || event?.turnId || '').trim()
    },
    forceChatState: true,
    activity: '正在处理',
    runningActivity: {
      id: 'thinking',
      title: '正在处理',
      detail: '等待模型响应',
      state: 'running'
    },
    removeActivityIds: ['thinking']
  };
}

export function streamTurnCompletedPatch(currentMessages, event) {
  const turnId = String(event?.turn_id || event?.turnId || '').trim();
  const messages = removeStreamingMessagesForTurn(currentMessages, turnId);
  return {
    resetStreamState: true,
    chatState: { state: 'idle' },
    activity: '已完成',
    messages,
    messagesChanged: messages !== currentMessages,
    shouldCache: messages !== currentMessages,
    clearRunningActivitiesDelay: 700
  };
}

export function streamAssistantDeltaPatch(currentMessages, event, scopeKey, streamBuffers) {
  const delta = streamDeltaText(event);
  if (!delta) return null;
  const messageId = streamActivityBaseId(event);
  const bufferKey = `${scopeKey}:assistant:${messageId}`;
  const text = streamBuffers?.append
    ? streamBuffers.append(bufferKey, delta)
    : undefined;
  return {
    chatState: { state: 'running', currentTurnState: event },
    messages: appendStreamAssistantDelta(currentMessages, event, text),
    activity: '正在回复',
    runningActivity: {
      id: `stream-assistant-${messageId}`,
      title: '正在回复',
      detail: shortText(delta, 72),
      state: 'running'
    },
    removeActivityIds: [`stream-assistant-${messageId}`, 'thinking']
  };
}

export function streamReasoningPartPatch(event) {
  const messageId = streamActivityBaseId(event);
  return {
    chatState: { state: 'running', currentTurnState: event },
    activity: '思考中',
    runningActivity: {
      id: `stream-reasoning-${messageId}`,
      title: '思考中',
      detail: '整理推理摘要',
      state: 'running'
    },
    removeActivityIds: [`stream-reasoning-${messageId}`, 'thinking']
  };
}

export function streamReasoningDeltaPatch(currentMessages, event, scopeKey, streamBuffers) {
  const messageId = streamActivityBaseId(event);
  const summaryIndex = event?.summary_index ?? event?.summaryIndex ?? 0;
  const bufferKey = `${scopeKey}:reasoning:${messageId}:${summaryIndex}`;
  const text = streamBuffers.append(bufferKey, streamDeltaText(event));
  return {
    chatState: { state: 'running', currentTurnState: event },
    messages: appendStreamReasoningSummary(currentMessages, event),
    activity: '思考中',
    runningActivity: {
      id: `stream-reasoning-${messageId}`,
      title: '思考中',
      detail: shortText(text || '整理推理摘要', 96),
      state: 'running'
    },
    removeActivityIds: [`stream-reasoning-${messageId}`, 'thinking']
  };
}

export function streamToolCallDeltaPatch(currentMessages, event, scopeKey, streamBuffers) {
  const itemId = streamItemId(event);
  const bufferKey = `${scopeKey}:tool:${itemId}`;
  const text = streamBuffers.append(bufferKey, streamDeltaText(event));
  return {
    chatState: { state: 'running', currentTurnState: event },
    messages: appendStreamToolCallDelta(currentMessages, event),
    activity: '准备调用工具',
    runningActivity: {
      id: `stream-tool-${itemId}`,
      title: '准备调用工具',
      detail: shortText(text, 96),
      state: 'running'
    },
    removeActivityIds: [`stream-tool-${itemId}`, 'thinking']
  };
}

export function streamToolResultDonePatch(currentMessages, event) {
  const toolResult = event?.tool_result || event?.toolResult || {};
  const itemId = String(toolResult.tool_call_id || toolResult.toolCallId || event?.batch_id || event?.batchId || streamItemId(event)).trim();
  const toolName = String(toolResult.tool_name || toolResult.toolName || '工具').trim();
  return {
    chatState: { state: 'running', currentTurnState: event },
    messages: appendStreamToolResultDone(currentMessages, event),
    activity: `${toolName} 已返回`,
    runningActivity: {
      id: `stream-tool-result-${itemId || toolName}`,
      title: `${toolName} 已返回`,
      detail: toolName,
      state: 'running'
    },
    removeActivityIds: [`stream-tool-${itemId}`, `stream-tool-result-${itemId}`, 'thinking']
  };
}

export function streamErrorPatch(currentMessages, event) {
  const messageId = streamActivityBaseId(event);
  const error = streamErrorText(event);
  return {
    chatState: { state: 'failed', lastError: error },
    messages: applyStreamErrorToMessages(currentMessages, event),
    activity: error,
    runningActivity: {
      id: `stream-error-${messageId}`,
      title: '响应失败',
      detail: shortText(error, 96),
      state: 'failed'
    },
    removeActivityIds: [`stream-assistant-${messageId}`, `stream-reasoning-${messageId}`, 'thinking']
  };
}
