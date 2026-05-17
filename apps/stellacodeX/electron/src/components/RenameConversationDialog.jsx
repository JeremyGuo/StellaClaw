import { useEffect, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';
import { displayConversationName } from '../lib/api';

export function RenameConversationDialog({ open, conversation, saving, onOpenChange, onRename }) {
  const [nickname, setNickname] = useState('');
  const currentName = displayConversationName(conversation || {});

  useEffect(() => {
    if (open) setNickname(currentName);
  }, [currentName, open]);

  const submit = (event) => {
    event.preventDefault();
    if (!conversation || saving) return;
    onRename?.(conversation, nickname.trim());
  };

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content className="dialog-content rename-conversation-dialog">
          <div className="dialog-titlebar rename-dialog-titlebar">
            <div className="rename-dialog-heading">
              <span className="rename-dialog-mark" aria-hidden="true">
                {String(currentName || 'C').slice(0, 1).toUpperCase()}
              </span>
              <div>
                <Dialog.Title>重命名 Conversation</Dialog.Title>
                <Dialog.Description>修改左侧文件夹显示名称。</Dialog.Description>
              </div>
            </div>
            <Dialog.Close className="dialog-close" type="button">×</Dialog.Close>
          </div>
          <form className="dialog-form rename-dialog-form" onSubmit={submit}>
            <label className="form-field rename-dialog-field">
              <span>Conversation 名称</span>
              <input
                autoFocus
                value={nickname}
                onChange={(event) => setNickname(event.target.value)}
                placeholder={currentName || 'Conversation'}
              />
            </label>
            <div className="dialog-actions rename-dialog-actions">
              <Dialog.Close className="secondary-button" type="button">取消</Dialog.Close>
              <button className="primary-button" type="submit" disabled={saving || !conversation || !nickname.trim()}>
                {saving ? '正在保存...' : '保存'}
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
