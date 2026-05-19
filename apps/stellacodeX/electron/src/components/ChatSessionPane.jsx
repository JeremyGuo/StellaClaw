import { ChatWorkspace } from './ChatWorkspace';
import { useChatRuntimeSnapshot } from '../lib/chatRuntimeStore';
import { hasOlderMessages } from '../lib/messageUtils';

export function ChatSessionPane(props) {
  const { messages, messagesReady, sending } = useChatRuntimeSnapshot();
  return (
    <ChatWorkspace
      {...props}
      messages={messages}
      messagesReady={messagesReady}
      hasOlder={hasOlderMessages(messages)}
      sending={sending}
    />
  );
}
