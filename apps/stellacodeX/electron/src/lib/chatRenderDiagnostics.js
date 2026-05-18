import { messageText } from './fileUtils';
import { buildChatRenderModel } from '../components/chat/renderModel';
import { isFinalAssistantMessage, messageIndex, splitMessageForDisplay, tokenUsage } from './messageUtils';

const TEXT_LIMIT = 1800;
const ITEM_TEXT_LIMIT = 900;
const MAX_ITEMS = 30;

export function chatRawRenderSnapshot(messages) {
  const source = Array.isArray(messages) ? messages : [];
  const { renderedMessages, renderEntries } = buildChatRenderModel({ messages: source });
  const rawMessages = source.slice(-MAX_ITEMS).map((message, index) => summarizeMessage(message, source.length - MAX_ITEMS + index));
  const displayMessages = renderedMessages.slice(-MAX_ITEMS).map((message, index) => summarizeDisplayMessage(message, renderedMessages.length - MAX_ITEMS + index));
  const entries = renderEntries.slice(-MAX_ITEMS).map((entry, index) => summarizeRenderEntry(entry, renderEntries.length - MAX_ITEMS + index));
  return {
    generated_at: new Date().toISOString(),
    counts: {
      raw_messages: source.length,
      display_messages: renderedMessages.length,
      render_entries: renderEntries.length
    },
    overview: {
      raw_messages: rawMessages.map(compactMessageLine),
      display_messages: displayMessages.map(compactDisplayLine),
      render_entries: entries.map(compactRenderLine)
    },
    raw_messages: rawMessages,
    display_messages: displayMessages,
    render_entries: entries
  };
}

export function chatRenderOverviewText(snapshot) {
  const lines = [];
  lines.push(`RAW messages (${snapshot.counts.raw_messages})`);
  lines.push(...(snapshot.overview?.raw_messages || []).map((line) => `  ${line}`));
  lines.push('');
  lines.push(`DISPLAY messages (${snapshot.counts.display_messages})`);
  lines.push(...(snapshot.overview?.display_messages || []).map((line) => `  ${line}`));
  lines.push('');
  lines.push(`RENDER entries (${snapshot.counts.render_entries})`);
  lines.push(...(snapshot.overview?.render_entries || []).map((line) => `  ${line}`));
  return lines.join('\n');
}

function summarizeDisplayMessage(message, index) {
  if (message?.type === 'toolGroup') {
    return {
      display_index: index,
      type: 'toolGroup',
      id: message.id,
      message_count: Array.isArray(message.messages) ? message.messages.length : 0,
      next_message: summarizeMessage(message.nextMessage),
      messages: (Array.isArray(message.messages) ? message.messages : []).map((item, itemIndex) => summarizeToolRoundMessage(item, itemIndex))
    };
  }
  return summarizeMessage(message, index);
}

function summarizeRenderEntry(entry, index) {
  if (entry?.type === 'assistantTurn') {
    return {
      render_index: index,
      type: 'assistantTurn',
      id: entry.id,
      process_group: summarizeDisplayMessage(entry.processGroup),
      final_message: summarizeMessage(entry.finalMessage)
    };
  }
  return {
    render_index: index,
    type: 'message',
    id: entry?.id,
    message: summarizeMessage(entry?.message)
  };
}

function summarizeToolRoundMessage(message, index) {
  const split = splitMessageForDisplay(message);
  return {
    ...summarizeMessage(message, index),
    split: {
      text_message: summarizeMessage(split.textMessage),
      tool_card_count: split.toolCards.length,
      segment_count: split.segments.length,
      segments: split.segments.map((segment, segmentIndex) => ({
        index: segmentIndex,
        notes: (segment.notes || []).map((note) => ({
          kind: note.kind,
          text_len: String(note.text || '').length,
          text: trimText(note.text, ITEM_TEXT_LIMIT)
        })),
        cards: (segment.cards || []).map((card) => ({
          id: card.id,
          kind: card.kind,
          name: card.name,
          payload_len: payloadText(card.payload).length,
          payload: trimText(payloadText(card.payload), ITEM_TEXT_LIMIT)
        }))
      }))
    }
  };
}

function summarizeMessage(message, index) {
  if (!message || typeof message !== 'object') return null;
  const text = messageText(message);
  const topText = topLevelMessageText(message);
  const items = Array.isArray(message.items) ? message.items : [];
  return compactObject({
    source_index: index,
    id: message.id || message.message_id || message.messageId,
    message_id: message.message_id || message.messageId,
    index: messageIndex(message),
    role: message.role,
    streaming: Boolean(message._streaming),
    stream_turn_id: message._streamTurnId,
    last_stream_event_index: message._lastStreamEventIndex,
    optimistic: Boolean(message._optimistic),
    pending: Boolean(message.pending),
    queued: Boolean(message.queued),
    final_assistant: isFinalAssistantMessage(message),
    execution_message: splitMessageForDisplay(message).toolCards.length > 0,
    text_len: text.length,
    text: trimText(text, TEXT_LIMIT),
    top_level_text_len: topText.length,
    top_level_text: trimText(topText, TEXT_LIMIT),
    item_count: items.length,
    items: items.map(summarizeItem),
    usage: tokenUsage(message)
  });
}

function compactMessageLine(message) {
  if (!message) return 'null';
  const topDiffers = message.top_level_text && message.top_level_text !== message.text;
  return [
    `#${message.source_index ?? '?'}`,
    message.role || 'message',
    message.streaming ? 'stream' : '',
    message.final_assistant ? 'final' : '',
    message.execution_message ? 'exec' : '',
    `idx=${message.index ?? '?'}`,
    `len=${message.text_len ?? 0}`,
    topDiffers ? `top_len=${message.top_level_text_len ?? 0}` : '',
    `items=${message.item_count ?? 0}`,
    message.id ? `id=${shortId(message.id)}` : '',
    `text=${oneLine(message.text, 150)}`,
    topDiffers ? `top=${oneLine(message.top_level_text, 150)}` : ''
  ].filter(Boolean).join(' ');
}

function compactDisplayLine(message) {
  if (!message) return 'null';
  if (message.type === 'toolGroup') {
    const messages = Array.isArray(message.messages) ? message.messages : [];
    const parts = messages.map((item) => {
      const split = item.split || {};
      const text = split.text_message?.text || item.text || '';
      const noteCount = (split.segments || []).reduce((sum, segment) => sum + Number(segment.notes?.length || 0), 0);
      return [
        `${item.role || 'assistant'}${item.streaming ? ':stream' : ''}`,
        `len=${item.text_len ?? 0}`,
        item.top_level_text && item.top_level_text !== item.text ? `top_len=${item.top_level_text_len ?? 0}` : '',
        `notes=${noteCount}`,
        `tools=${split.tool_card_count ?? 0}`,
        `text=${oneLine(text, 120)}`,
        item.top_level_text && item.top_level_text !== item.text ? `top=${oneLine(item.top_level_text, 120)}` : ''
      ].filter(Boolean).join(' ');
    });
    return [
      `#${message.display_index ?? '?'}`,
      'toolGroup',
      `messages=${message.message_count ?? 0}`,
      `id=${shortId(message.id)}`,
      parts.length ? `=> ${parts.join(' | ')}` : ''
    ].filter(Boolean).join(' ');
  }
  return compactMessageLine({ ...message, source_index: message.source_index ?? message.display_index });
}

function compactRenderLine(entry) {
  if (!entry) return 'null';
  if (entry.type === 'assistantTurn') {
    const group = entry.process_group || {};
    return [
      `#${entry.render_index ?? '?'}`,
      'assistantTurn',
      `id=${shortId(entry.id)}`,
      compactDisplayLine(group),
      entry.final_message ? `final=${oneLine(entry.final_message.text, 120)}` : ''
    ].filter(Boolean).join(' ');
  }
  return [
    `#${entry.render_index ?? '?'}`,
    'message',
    compactMessageLine(entry.message)
  ].filter(Boolean).join(' ');
}

function oneLine(value, max = 120) {
  return trimText(value, max).replace(/\s+/g, ' ').trim();
}

function shortId(value) {
  const text = String(value || '');
  if (text.length <= 18) return text;
  return `${text.slice(0, 10)}…${text.slice(-6)}`;
}

function summarizeItem(item, index) {
  if (!item || typeof item !== 'object') return item;
  const text = itemText(item);
  return compactObject({
    index,
    type: item.type,
    item_index: item.index,
    tool_call_id: item.tool_call_id || item.call_id || item.item_id,
    tool_name: item.tool_name || item.name,
    text_len: text.length,
    text: trimText(text, ITEM_TEXT_LIMIT),
    argument_len: String(item.arguments || '').length,
    arguments: trimText(item.arguments, ITEM_TEXT_LIMIT),
    context_len: String(item.context_with_attachment_markers || item.context || '').length,
    context: trimText(item.context_with_attachment_markers || item.context, ITEM_TEXT_LIMIT),
    has_structured: Boolean(item.structured)
  });
}

function topLevelMessageText(message) {
  return String(
    message?.text_with_attachment_markers
    || message?.rendered_text
    || message?.text
    || message?.content
    || message?.preview
    || ''
  );
}

function itemText(item) {
  if (typeof item === 'string') return item;
  if (!item || typeof item !== 'object') return '';
  const payload = item.payload && typeof item.payload === 'object' ? item.payload : {};
  return String(
    item.text_with_attachment_markers
    || item.text
    || item.content
    || item.summary
    || payload.text
    || payload.codex_summary
    || ''
  );
}

function payloadText(value) {
  if (typeof value === 'string') return value;
  if (value === undefined || value === null) return '';
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function trimText(value, max = TEXT_LIMIT) {
  const text = payloadText(value);
  return text.length > max ? `${text.slice(0, max)}...` : text;
}

function compactObject(object) {
  return Object.fromEntries(
    Object.entries(object || {}).filter(([, value]) => (
      value !== undefined
      && value !== null
      && value !== ''
      && !(Array.isArray(value) && value.length === 0)
      && !(typeof value === 'number' && !Number.isFinite(value))
    ))
  );
}
