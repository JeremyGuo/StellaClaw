import { useEffect, useMemo, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';

export function NewConversationDialog({ open, servers = [], activeServerId, creating, onOpenChange, onCreate }) {
  const initialServerId = activeServerId || servers[0]?.id || '';
  const [serverId, setServerId] = useState(initialServerId);
  const [nickname, setNickname] = useState('');
  const selectedServer = useMemo(
    () => servers.find((server) => server.id === serverId) || servers[0],
    [servers, serverId]
  );

  useEffect(() => {
    if (open) {
      setServerId(activeServerId || servers[0]?.id || '');
      setNickname('');
    }
  }, [activeServerId, open, servers]);

  const submit = (event) => {
    event.preventDefault();
    if (!selectedServer || creating) return;
    onCreate?.({ serverId: selectedServer.id, nickname });
  };

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content className="dialog-content small">
          <div className="dialog-titlebar">
            <div>
              <Dialog.Title>新建 Conversation</Dialog.Title>
              <Dialog.Description>选择上游服务器。模型会在进入对话后初始化。</Dialog.Description>
            </div>
            <Dialog.Close className="dialog-close" type="button">×</Dialog.Close>
          </div>
          <form className="dialog-form" onSubmit={submit}>
            <label className="form-field">
              <span>Nickname</span>
              <input
                value={nickname}
                onChange={(event) => setNickname(event.target.value)}
                placeholder="可选"
              />
            </label>
            <label className="form-field">
              <span>服务器</span>
              <select value={selectedServer?.id || ''} onChange={(event) => setServerId(event.target.value)}>
                {servers.map((server) => (
                  <option key={server.id} value={server.id}>{server.name || server.id}</option>
                ))}
              </select>
            </label>
            <div className="server-preview">
              <strong>{selectedServer?.name || '未配置服务器'}</strong>
              <span>{selectedServer?.connectionMode === 'ssh_proxy' ? selectedServer.targetUrl : selectedServer?.baseUrl}</span>
            </div>
            <div className="dialog-actions">
              <Dialog.Close className="secondary-button" type="button">取消</Dialog.Close>
              <button className="primary-button" type="submit" disabled={!selectedServer || creating}>
                {creating ? '正在创建...' : '创建'}
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
