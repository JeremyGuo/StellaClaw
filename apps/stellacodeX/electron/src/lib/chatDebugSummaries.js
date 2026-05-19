import { messageText } from './fileUtils';
import { messageIndex } from './messageUtils';
import { streamEventType } from './chatStreamDataPlane';

export function streamDeltaSummary(event) {
  const delta = String(event?.delta ?? event?.text_delta ?? event?.textDelta ?? '');
  return {
    eventType: streamEventType(event),
    message_id: event?.message_id || event?.messageId || event?.stream_id || event?.streamId,
    item_id: event?.item_id || event?.itemId,
    call_id: event?.call_id || event?.callId,
    tool_name: event?.tool_name || event?.toolName,
    in_message_index: event?.in_message_index ?? event?.inMessageIndex,
    delta_len: delta.length,
    delta: shortLogText(delta)
  };
}

export function patchActionSummary(type, patch) {
  if (type === 'stream_assistant_message_delta') return 'append assistant delta';
  if (type === 'stream_reasoning_summary_delta') return 'append reasoning delta';
  if (type === 'stream_reasoning_summary_part_added') return 'start reasoning summary';
  if (type === 'stream_tool_call_delta') return 'append tool call delta';
  if (type === 'stream_tool_result_done') return 'append tool result / refresh message';
  if (type === 'stream_turn_start' || type === 'turn_started') return 'turn started';
  if (type === 'stream_turn_done' || type === 'turn_completed') return 'turn done / remove provisional stream';
  if (type === 'stream_error') return 'drop current provisional stream';
  return patch?.messages ? 'refresh messages' : 'update state';
}

export function streamUiCategory(type, patch) {
  if (!patch?.messages) return 'stream';
  if (type === 'stream_turn_done' || type === 'turn_completed' || type === 'stream_error') {
    return 'replace_ui_element';
  }
  return 'append_stream_to_ui';
}

export function streamUiKind(category) {
  if (category === 'append_stream_to_ui') return 'chat.append_stream_to_ui';
  if (category === 'replace_ui_element') return 'chat.replace_ui_element';
  return 'chat.stream';
}

export function compactMessageSummary(message) {
  if (!message) return null;
  const text = messageText(message);
  return {
    id: message.id || message.message_id,
    index: messageIndex(message),
    role: message.role,
    streaming: Boolean(message._streaming),
    text_len: text.length,
    text: shortLogText(text),
    item_types: (Array.isArray(message.items) ? message.items : [])
      .map((item) => item?.type)
      .filter(Boolean)
      .slice(0, 8)
  };
}

export function compactMessagesSummary(messages, count = 3) {
  return (Array.isArray(messages) ? messages : []).slice(-count).map(compactMessageSummary);
}

function shortLogText(value, max = 120) {
  const text = String(value || '').replace(/\s+/g, ' ').trim();
  return text.length > max ? `${text.slice(0, max)}...` : text;
}
