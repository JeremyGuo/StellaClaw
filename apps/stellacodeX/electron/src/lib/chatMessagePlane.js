import {
  activityFromMessages,
  committedMessageProtocolMismatches,
  isFinalAssistantMessage,
  lastServerMessageIndex,
  mergeMessages,
  messageIndex,
  usageDeltaFromMessages
} from './messageUtils';
import {
  appendStreamToolResultDone,
  markQueuedUserMessage,
  removeStreamingMessagesForTurn,
  streamFinalizedActivityIds
} from './chatStreamDataPlane';
import { chatSnapshotState, recentMessagePageParams } from './chatSessionState';

export function incomingMessagesPatch(currentMessages, incoming, scopeKey, seenUsageMessages) {
  if (!Array.isArray(incoming) || incoming.length === 0) return null;
  const protocolMismatches = committedMessageProtocolMismatches(currentMessages, incoming);
  const finalizedActivities = streamFinalizedActivityIds(incoming);
  const usageDelta = usageDeltaFromMessages(scopeKey, incoming, seenUsageMessages);
  const nextMessages = mergeMessages(currentMessages, incoming);
  const latestMessage = latestMessageByIndex(incoming);
  const latestId = latestMessage?.id ?? latestMessage?.message_id;
  const latestIndex = latestMessage ? messageIndex(latestMessage) : undefined;
  return {
    messages: nextMessages,
    protocolMismatches,
    finalizedActivities,
    usageDelta,
    latestMessage,
    latestId,
    latestIndex,
    activity: activityFromMessages(incoming),
    hasFinalAssistant: incoming.some((message) => isFinalAssistantMessage(message))
  };
}

export function recentMessagesPatch(incoming) {
  if (!Array.isArray(incoming) || incoming.length === 0) return null;
  const latestMessage = latestMessageByIndex(incoming);
  const latestId = latestMessage?.id ?? latestMessage?.message_id;
  const latestIndex = latestMessage ? messageIndex(latestMessage) : undefined;
  return {
    messages: incoming,
    latestMessage,
    latestId,
    latestIndex,
    activity: activityFromMessages(incoming)
  };
}

export function chatAckHistoryPlan(currentMessages, ack) {
  const total = Number(ack?.next_message_index ?? ack?.total ?? ack?.next_message_id);
  if (!Number.isFinite(total)) return { kind: 'none' };
  const lastIndex = lastServerMessageIndex(currentMessages);
  if (lastIndex === undefined) {
    if (total <= 0) return { kind: 'clear' };
    return {
      kind: 'fetch',
      params: recentMessagePageParams(null, 40, total),
      replace: false
    };
  }
  if (total <= lastIndex + 1) return { kind: 'none' };
  const gap = total - lastIndex - 1;
  const replace = gap > 200;
  return {
    kind: 'fetch',
    params: replace
      ? recentMessagePageParams(null, 80, total)
      : { offset: lastIndex + 1, limit: gap },
    replace
  };
}

export function chatSnapshotProjection(currentMessages, snapshot) {
  if (!snapshot) return null;
  const snapshotState = chatSnapshotState(snapshot);
  const provisional = snapshot.current_provisional_assistant_message?.message;
  const toolStates = Array.isArray(snapshot.running_tool_results)
    ? snapshot.running_tool_results
    : [];
  const queuedMessages = Array.isArray(snapshot.queued_outbound_messages)
    ? snapshot.queued_outbound_messages
    : [];

  let messages = currentMessages;
  let changed = false;
  let activity = '';
  let clearRunningActivities = false;
  let runningActivities = null;

  if (provisional) {
    const turnId = String(snapshot.current_turn_state?.turn_id || snapshot.current_turn_state?.turnId || '').trim();
    const projected = {
      ...provisional,
      id: provisional.id || provisional.message_id || provisional.messageId || turnId || undefined,
      message_id: provisional.message_id || provisional.messageId || provisional.id || turnId || undefined,
      _streamTurnId: turnId || provisional._streamTurnId || '',
      _streaming: true
    };
    messages = mergeMessages(messages, [projected]);
    changed = true;
    activity = '正在回复';
  }

  if (queuedMessages.length > 0) {
    let next = messages;
    queuedMessages.forEach((queued) => {
      next = markQueuedUserMessage(next, queued?.client_message_id || queued?.clientMessageId);
    });
    if (next !== messages) {
      messages = next;
      changed = true;
    }
  }

  const activities = toolStates
    .filter((state) => !state?.committed)
    .map((state) => state?.tool_result || state?.toolResult || state)
    .filter(Boolean)
    .map((toolResult) => {
      const itemId = String(toolResult.tool_call_id || toolResult.toolCallId || toolResult.tool_name || toolResult.toolName || '').trim();
      const toolName = String(toolResult.tool_name || toolResult.toolName || '工具').trim();
      return {
        id: `stream-tool-result-${itemId || toolName}`,
        title: `${toolName} 已返回`,
        detail: toolName,
        state: 'running'
      };
    });

  if (activities.length > 0) {
    let next = messages;
    toolStates.forEach((state) => {
      if (state?.committed) return;
      next = appendStreamToolResultDone(next, {
        turn_id: state?.turn_id || state?.turnId || snapshot.current_turn_state?.turn_id,
        tool_result: state?.tool_result || state?.toolResult || state
      });
    });
    messages = next;
    changed = true;
    runningActivities = activities;
    activity = activities[activities.length - 1]?.title || '正在处理';
  } else if (snapshotState.state === 'running' && !provisional) {
    runningActivities = [{
      id: 'thinking',
      title: '正在处理',
      detail: '等待模型响应',
      state: 'running'
    }];
    activity = '正在处理';
  } else if (snapshotState.state === 'idle') {
    const next = removeStreamingMessagesForTurn(messages);
    if (next !== messages) {
      messages = next;
      changed = true;
    }
    clearRunningActivities = true;
  }

  return {
    messages,
    changed,
    shouldCache: changed && snapshotState.state === 'idle',
    activity,
    runningActivities,
    clearRunningActivities,
    snapshotState
  };
}

function latestMessageByIndex(messages) {
  return messages.reduce((latest, message) => (
    !latest || messageIndex(message) >= messageIndex(latest) ? message : latest
  ), null);
}
