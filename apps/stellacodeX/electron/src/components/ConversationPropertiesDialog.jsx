import { useEffect, useMemo, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';
import { displayConversationName, displayForegroundSessionName, foregroundSessions } from '../lib/api';
import { formatModel, modelAlias, modelDisplayName } from '../lib/format';

const REASONING_EFFORTS = [
  { value: 'low', label: 'Low' },
  { value: 'medium', label: 'Medium' },
  { value: 'high', label: 'High' },
  { value: 'xhigh', label: 'XHigh' },
  { value: 'default', label: 'Default' }
];

const IDLE_COMPACT_OPTIONS = [
  { value: 'default', label: '继承模型默认' },
  { value: 'on', label: '开启' },
  { value: 'off', label: '关闭' }
];

export function ConversationPropertiesDialog({
  open,
  conversation,
  status,
  models = [],
  modelsLoading = false,
  modelsError = '',
  applying = false,
  onOpenChange,
  onLoadModels,
  onSwitchModel,
  onSwitchReasoning,
  onSwitchIdleCompact
}) {
  const [selectedModel, setSelectedModel] = useState('');
  const [selectedReasoning, setSelectedReasoning] = useState('default');
  const [selectedIdleCompact, setSelectedIdleCompact] = useState('default');
  const sessions = useMemo(() => foregroundSessions(conversation || {}), [conversation]);
  const currentModel = formatModel(conversation || {}, status || {}) || 'pending';
  const currentReasoning = String(conversation?.reasoning || status?.reasoning || 'default');
  const currentIdleCompact = idleCompactState(conversation, status);
  const title = displayConversationName(conversation || {});
  const stats = useMemo(() => conversationStats(conversation, sessions), [conversation, sessions]);

  useEffect(() => {
    if (!open) return;
    setSelectedModel('');
    setSelectedReasoning(currentReasoning || 'default');
    setSelectedIdleCompact(currentIdleCompact.override);
    onLoadModels?.();
  }, [currentIdleCompact.override, currentReasoning, onLoadModels, open]);

  const applyModel = (event) => {
    event.preventDefault();
    if (!selectedModel) return;
    onSwitchModel?.(conversation, selectedModel);
  };

  const applyReasoning = (event) => {
    event.preventDefault();
    if (!selectedReasoning) return;
    onSwitchReasoning?.(conversation, selectedReasoning);
  };

  const applyIdleCompact = (event) => {
    event.preventDefault();
    if (!selectedIdleCompact) return;
    onSwitchIdleCompact?.(conversation, selectedIdleCompact);
  };

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content className="dialog-content conversation-properties-dialog">
          <div className="dialog-titlebar">
            <div>
              <Dialog.Title>Conversation 属性</Dialog.Title>
              <Dialog.Description>{title || 'Conversation'}</Dialog.Description>
            </div>
            <Dialog.Close className="dialog-close" type="button">×</Dialog.Close>
          </div>
          <div className="conversation-properties-body">
            <section className="properties-section">
              <h3>统计</h3>
              <dl className="properties-grid">
                <dt>Sessions</dt><dd>{sessions.length}</dd>
                <dt>Messages</dt><dd>{stats.messages}</dd>
                <dt>Running</dt><dd>{stats.running}</dd>
                <dt>Last activity</dt><dd>{stats.lastActivity || '-'}</dd>
                <dt>Conversation ID</dt><dd title={conversation?.conversation_id}>{conversation?.conversation_id || '-'}</dd>
              </dl>
            </section>

            <section className="properties-section">
              <h3>当前设置</h3>
              <dl className="properties-grid">
                <dt>Model</dt><dd>{currentModel}</dd>
                <dt>Reasoning</dt><dd>{currentReasoning || 'default'}</dd>
                <dt>Idle compact</dt><dd>{currentIdleCompact.summary}</dd>
                <dt>Remote</dt><dd>{conversation?.remote || status?.remote || 'local'}</dd>
                <dt>Sandbox</dt><dd>{conversation?.sandbox || status?.sandbox || '-'}</dd>
                <dt>Workspace</dt><dd title={conversation?.workspace || status?.workspace}>{conversation?.workspace || status?.workspace || '-'}</dd>
              </dl>
            </section>

            <section className="properties-section">
              <h3>Sessions</h3>
              <div className="properties-session-list">
                {sessions.map((session) => (
                  <div className="properties-session-row" key={session.id || session.foreground_session_id}>
                    <strong>{displayForegroundSessionName(session, conversation)}</strong>
                    <span>{session.state || session.processing_state || 'idle'}</span>
                    <em>{session.message_count || 0} messages</em>
                  </div>
                ))}
              </div>
            </section>

            <section className="properties-section">
              <h3>切换模型</h3>
              <form className="properties-action-row" onSubmit={applyModel}>
                <select value={selectedModel} onChange={(event) => setSelectedModel(event.target.value)} disabled={modelsLoading || applying}>
                  <option value="">{modelsLoading ? '正在读取模型...' : '选择模型'}</option>
                  {models.map((model) => (
                    <option key={modelAlias(model)} value={modelAlias(model)}>
                      {[modelAlias(model), modelDisplayName(model)].filter(Boolean).join(' - ')}
                    </option>
                  ))}
                </select>
                <button className="primary-button" type="submit" disabled={!selectedModel || applying || modelsLoading}>应用</button>
              </form>
              {modelsError && <p className="properties-error">{modelsError}</p>}
            </section>

            <section className="properties-section">
              <h3>Reasoning</h3>
              <form className="properties-action-row" onSubmit={applyReasoning}>
                <select value={selectedReasoning} onChange={(event) => setSelectedReasoning(event.target.value)} disabled={applying}>
                  {REASONING_EFFORTS.map((effort) => (
                    <option key={effort.value} value={effort.value}>{effort.label}</option>
                  ))}
                </select>
                <button className="primary-button" type="submit" disabled={!selectedReasoning || applying}>应用</button>
              </form>
            </section>

            <section className="properties-section">
              <h3>Idle timeout compact</h3>
              <form className="properties-action-row" onSubmit={applyIdleCompact}>
                <select value={selectedIdleCompact} onChange={(event) => setSelectedIdleCompact(event.target.value)} disabled={applying}>
                  {IDLE_COMPACT_OPTIONS.map((option) => (
                    <option key={option.value} value={option.value}>{option.label}</option>
                  ))}
                </select>
                <button className="primary-button" type="submit" disabled={!selectedIdleCompact || applying}>应用</button>
              </form>
              <p className="properties-help">
                当前{currentIdleCompact.enabled ? '开启' : '关闭'}，{currentIdleCompact.sourceLabel}。
              </p>
            </section>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function idleCompactState(conversation, status) {
  const overrideRaw = conversation?.idle_timeout_compact_override
    ?? conversation?.runtime_config?.idle_timeout_compact_override
    ?? conversation?.runtime_config?.idle_timeout_compact_enabled
    ?? status?.idle_timeout_compact_override
    ?? status?.runtime_config?.idle_timeout_compact_override
    ?? status?.runtime_config?.idle_timeout_compact_enabled
    ?? null;
  const effectiveRaw = conversation?.idle_timeout_compact_enabled
    ?? conversation?.runtime_config?.idle_timeout_compact_effective
    ?? conversation?.runtime_config?.idle_timeout_compact_enabled
    ?? status?.idle_timeout_compact_enabled
    ?? status?.runtime_config?.idle_timeout_compact_effective
    ?? status?.runtime_config?.idle_timeout_compact_enabled
    ?? true;
  const override = overrideRaw === true ? 'on' : overrideRaw === false ? 'off' : 'default';
  const enabled = effectiveRaw !== false;
  const sourceLabel = override === 'default' ? '继承模型默认值' : 'Conversation 覆盖';
  return {
    override,
    enabled,
    sourceLabel,
    summary: `${enabled ? '开启' : '关闭'} · ${sourceLabel}`
  };
}

function conversationStats(conversation, sessions) {
  const messageCount = sessions.reduce((total, session) => total + Number(session.message_count || 0), 0);
  const runningCount = sessions.filter((session) => {
    const state = String(session.state || session.processing_state || '').toLowerCase();
    return session.running || state === 'running' || state === 'queued';
  }).length;
  const lastActivity = sessions
    .map((session) => session.last_message_time || session.last_activity_at || session.updated_at)
    .filter(Boolean)
    .sort()
    .at(-1)
    || conversation?.last_message_time
    || conversation?.updated_at
    || '';
  return {
    messages: messageCount || Number(conversation?.message_count || 0),
    running: runningCount,
    lastActivity
  };
}
