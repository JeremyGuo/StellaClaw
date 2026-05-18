import { connectionInfo } from './api';
import { websocketUrl } from './messageUtils';

export function startChatSocketClient({
  serverId,
  conversationId,
  foregroundSessionId,
  isCurrent,
  onPayload,
  onStatus,
  onFallback
}) {
  let closed = false;
  let socket = null;
  let reconnectTimer = null;

  const active = () => !closed && (typeof isCurrent !== 'function' || isCurrent());

  const close = () => {
    closed = true;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    const currentSocket = socket;
    socket = null;
    if (currentSocket && currentSocket.readyState <= WebSocket.OPEN) {
      currentSocket.close();
    }
  };

  const connect = async () => {
    try {
      const info = await connectionInfo(serverId);
      if (!active()) return;
      const nextSocket = new WebSocket(websocketUrl(info.baseUrl, info.token, conversationId, foregroundSessionId));
      if (!active()) {
        nextSocket.close();
        return;
      }
      socket = nextSocket;
      nextSocket.addEventListener('message', (event) => {
        if (!active()) return;
        let payload;
        try {
          payload = JSON.parse(event.data);
        } catch {
          return;
        }
        onPayload?.(payload);
      });
      nextSocket.addEventListener('close', () => {
        if (!active()) return;
        reconnectTimer = setTimeout(connect, 2000);
        onStatus?.('reconnecting');
      });
      nextSocket.addEventListener('error', () => {
        if (active()) onStatus?.('error');
      });
    } catch {
      if (!active()) return;
      onStatus?.('unavailable');
      onFallback?.();
    }
  };

  connect();

  return { close };
}
