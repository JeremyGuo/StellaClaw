import { messageIndex } from './messageUtils';
import { messageText } from './fileUtils';

const STORAGE_KEY = 'stellacode.chatProtocolDiagnostics.v3';
const VERBOSE_KEY = 'stellacode.chatProtocolDiagnostics.verbose';
const MAX_RECORDS = 300;

export function recordChatProtocolDiagnostic(kind, details = {}) {
  if (!shouldRecordChatProtocolDiagnostic(kind, details?.category)) return;
  const record = {
    time: new Date().toISOString(),
    kind,
    ...sanitize(details)
  };
  writeLocalRecord(record);
  emitDiagnosticsEvent(record);
  if (typeof window !== 'undefined' && window.stellacode2?.appendProtocolLog) {
    window.stellacode2.appendProtocolLog(record).catch(() => {});
  }
  if (kind.includes('mismatch') || kind.includes('warning') || kind.includes('gap')) {
    console.warn(`[chat-protocol] ${kind}`, record);
  }
}

export function shouldRecordChatProtocolDiagnostic(kind, category = '') {
  const value = String(kind || '');
  if (
    value.includes('mismatch')
    || value.includes('warning')
    || value.includes('gap')
    || value.includes('error')
    || String(category || '').includes('error')
  ) {
    return true;
  }
  if (typeof window === 'undefined') return false;
  if (window.__stellacodeProtocolLogOpen) return true;
  try {
    return window.localStorage?.getItem(VERBOSE_KEY) === '1';
  } catch {
    return false;
  }
}

export function readChatProtocolDiagnostics() {
  if (typeof window === 'undefined' || !window.localStorage) return [];
  try {
    const records = JSON.parse(window.localStorage.getItem(STORAGE_KEY) || '[]');
    return Array.isArray(records) ? records : [];
  } catch {
    return [];
  }
}

export function clearChatProtocolDiagnostics() {
  if (typeof window === 'undefined' || !window.localStorage) return;
  try {
    window.localStorage.removeItem(STORAGE_KEY);
    window.dispatchEvent(new CustomEvent('stellacode:protocol-log-cleared'));
  } catch {
    // Diagnostics must never affect chat rendering.
  }
}

export function summarizePayload(payload) {
  const type = String(payload?.type || '');
  const event = payload?.event || payload?.session_event || payload?.stream_event || payload;
  const message = payload?.message || payload?.current_provisional_assistant_message?.message || null;
  const toolResult = event?.tool_result || event?.toolResult || payload?.tool_result || payload?.toolResult || null;
  return compactObject({
    type,
    reason: payload?.reason,
    committed: payload?.committed,
    next_message_index: payload?.next_message_index ?? payload?.nextMessageIndex,
    next_message_id: payload?.next_message_id ?? payload?.nextMessageId,
    message: summarizeMessage(message),
    event: type.startsWith('chat.stream_') || type === 'chat.plan_updated'
      ? summarizeStreamEvent(event)
      : undefined,
    snapshot: type === 'chat.snapshot' ? summarizeSnapshot(payload) : undefined,
    tool_result: toolResult ? summarizeToolResult(toolResult) : undefined,
    error: payload?.error || payload?.message
  });
}

export function summarizeMessagesTail(messages, count = 8) {
  return (Array.isArray(messages) ? messages : [])
    .slice(-count)
    .map(summarizeMessage);
}

export function summarizeStreamingAssistants(messages) {
  return (Array.isArray(messages) ? messages : [])
    .filter((message) => (
      message?._streaming
      && String(message?.role || '').toLowerCase() === 'assistant'
    ))
    .map(summarizeMessage);
}

export function summarizeMessage(message) {
  if (!message || typeof message !== 'object') return null;
  const items = Array.isArray(message.items) ? message.items : [];
  return compactObject({
    id: message.id || message.message_id || message.messageId,
    index: messageIndex(message),
    role: message.role,
    streaming: Boolean(message._streaming),
    optimistic: Boolean(message._optimistic),
    pending: Boolean(message.pending),
    queued: Boolean(message.queued),
    turn_id: message._streamTurnId || message.turn_id || message.turnId,
    text_len: messageText(message).length,
    text_head: trimForLog(messageText(message), 120),
    item_count: items.length,
    item_types: items.map((item) => item?.type).filter(Boolean).slice(0, 12),
    tool_items: items
      .filter((item) => item?.type === 'tool_call' || item?.type === 'tool_result')
      .slice(0, 8)
      .map((item) => compactObject({
        type: item.type,
        id: item.tool_call_id || item.call_id || item.item_id,
        name: item.tool_name || item.name,
        args_len: String(item.arguments || '').length,
        context_len: String(item.context || item.context_with_attachment_markers || '').length
      }))
  });
}

function summarizeStreamEvent(event) {
  if (!event || typeof event !== 'object') return null;
  const delta = String(event.delta ?? event.text_delta ?? event.textDelta ?? '');
  return compactObject({
    type: event.type || event.event_type || event.kind,
    turn_id: event.turn_id || event.turnId,
    message_id: event.message_id || event.messageId || event.stream_id || event.streamId,
    in_message_index: event.in_message_index ?? event.inMessageIndex,
    item_id: event.item_id || event.itemId,
    call_id: event.call_id || event.callId,
    tool_name: event.tool_name || event.toolName,
    delta_len: delta.length,
    delta_head: trimForLog(delta, 100),
    error: event.error || event.message || event.error_detail || event.errorDetail,
    tool_result: summarizeToolResult(event.tool_result || event.toolResult)
  });
}

function summarizeSnapshot(snapshot) {
  const provisional = snapshot?.current_provisional_assistant_message?.message;
  const queued = Array.isArray(snapshot?.queued_outbound_messages)
    ? snapshot.queued_outbound_messages
    : [];
  const toolStates = Array.isArray(snapshot?.running_tool_results)
    ? snapshot.running_tool_results
    : [];
  return compactObject({
    current_turn_id: snapshot?.current_turn_state?.turn_id || snapshot?.current_turn_state?.turnId,
    queued_count: queued.length,
    running_tool_count: toolStates.filter((state) => !state?.committed).length,
    provisional: summarizeMessage(provisional)
  });
}

function summarizeToolResult(toolResult) {
  if (!toolResult || typeof toolResult !== 'object') return null;
  const result = toolResult.result || {};
  const context = result.context?.text || toolResult.context || toolResult.context_with_attachment_markers || '';
  return compactObject({
    tool_call_id: toolResult.tool_call_id || toolResult.toolCallId,
    tool_name: toolResult.tool_name || toolResult.toolName,
    context_len: String(context || '').length,
    context_head: trimForLog(context, 100),
    has_structured: Boolean(result.structured || toolResult.structured),
    file_count: Array.isArray(result.files) ? result.files.length : undefined
  });
}

function writeLocalRecord(record) {
  if (typeof window === 'undefined' || !window.localStorage) return;
  try {
    const current = JSON.parse(window.localStorage.getItem(STORAGE_KEY) || '[]');
    const next = [...(Array.isArray(current) ? current : []), record].slice(-MAX_RECORDS);
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Diagnostics must never affect chat rendering.
  }
}

function emitDiagnosticsEvent(record) {
  if (typeof window === 'undefined') return;
  try {
    window.dispatchEvent(new CustomEvent('stellacode:protocol-log', { detail: record }));
  } catch {
    // Diagnostics must never affect chat rendering.
  }
}

function sanitize(value) {
  if (Array.isArray(value)) return value.map(sanitize).slice(0, 40);
  if (!value || typeof value !== 'object') return value;
  return compactObject(Object.fromEntries(
    Object.entries(value).map(([key, item]) => [key, sanitize(item)])
  ));
}

function compactObject(object) {
  return Object.fromEntries(
    Object.entries(object || {}).filter(([, value]) => (
      value !== undefined
      && value !== null
      && value !== ''
      && !(Array.isArray(value) && value.length === 0)
    ))
  );
}

function trimForLog(value, maxLength) {
  const text = String(value || '');
  if (text.length <= maxLength) return text;
  return `${text.slice(0, maxLength)}...`;
}
