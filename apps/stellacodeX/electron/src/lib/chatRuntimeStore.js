import { useSyncExternalStore } from 'react';

let state = {
  messages: [],
  messagesReady: false,
  sending: false
};

const listeners = new Set();

function emit() {
  for (const listener of listeners) listener();
}

function updateState(patch) {
  const next = { ...state, ...patch };
  if (
    next.messages === state.messages &&
    next.messagesReady === state.messagesReady &&
    next.sending === state.sending
  ) {
    return state;
  }
  state = next;
  emit();
  return state;
}

export function subscribeChatRuntime(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function getChatRuntimeSnapshot() {
  return state;
}

export function useChatRuntimeSnapshot() {
  return useSyncExternalStore(subscribeChatRuntime, getChatRuntimeSnapshot, getChatRuntimeSnapshot);
}

export function setChatRuntimeMessages(updater) {
  const next = typeof updater === 'function' ? updater(state.messages) : updater;
  return updateState({ messages: Array.isArray(next) ? next : [] }).messages;
}

export function setChatRuntimeMessagesReady(value) {
  return updateState({ messagesReady: Boolean(value) }).messagesReady;
}

export function setChatRuntimeSending(value) {
  return updateState({ sending: Boolean(value) }).sending;
}
