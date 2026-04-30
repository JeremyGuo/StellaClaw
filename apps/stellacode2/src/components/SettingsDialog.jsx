import { useEffect, useMemo, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';

const AUTHOR = 'Stellacode contributors';

function blankServer(index) {
  return {
    id: `server-${index}`,
    name: `Server ${index}`,
    baseUrl: 'http://127.0.0.1:3111',
    token: ''
  };
}

export function SettingsDialog({ open, settings, saving, onOpenChange, onSave }) {
  const [tab, setTab] = useState('appearance');
  const [draft, setDraft] = useState(settings);
  const [appVersion, setAppVersion] = useState('');

  useEffect(() => {
    if (open) setDraft(settings);
  }, [open, settings]);

  useEffect(() => {
    if (!open) return;
    window.stellacode2?.appVersion?.()
      .then((version) => setAppVersion(version || ''))
      .catch(() => {});
  }, [open]);

  const servers = useMemo(() => draft?.servers || [], [draft]);

  const updateServer = (serverId, patch) => {
    setDraft((current) => ({
      ...current,
      servers: (current?.servers || []).map((server) => (
        server.id === serverId ? { ...server, ...patch } : server
      ))
    }));
  };

  const addServer = () => {
    setDraft((current) => {
      const next = blankServer((current?.servers || []).length + 1);
      return {
        ...current,
        servers: [...(current?.servers || []), next]
      };
    });
  };

  const removeServer = (serverId) => {
    setDraft((current) => {
      const nextServers = (current?.servers || []).filter((server) => server.id !== serverId);
      return {
        ...current,
        servers: nextServers,
        activeServerId: current?.activeServerId === serverId ? nextServers[0]?.id || '' : current?.activeServerId
      };
    });
  };

  const save = () => {
    if (!draft || saving) return;
    onSave?.(draft);
  };

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="dialog-overlay" />
        <Dialog.Content className="dialog-content settings-dialog">
          <div className="dialog-titlebar">
            <div>
              <Dialog.Title>设置</Dialog.Title>
              <Dialog.Description>管理上游服务器、更新和关于信息。</Dialog.Description>
            </div>
            <Dialog.Close className="dialog-close" type="button">×</Dialog.Close>
          </div>
          <div className="settings-layout">
            <nav className="settings-tabs">
              <button className={tab === 'appearance' ? 'active' : ''} type="button" onClick={() => setTab('appearance')}>外观</button>
              <button className={tab === 'servers' ? 'active' : ''} type="button" onClick={() => setTab('servers')}>服务器</button>
              <button className={tab === 'update' ? 'active' : ''} type="button" onClick={() => setTab('update')}>更新</button>
              <button className={tab === 'about' ? 'active' : ''} type="button" onClick={() => setTab('about')}>关于</button>
            </nav>
            <section className="settings-pane">
              {tab === 'appearance' && (
                <div className="settings-card">
                  <strong>外观</strong>
                  <p>默认跟随系统，也可以手动固定为浅色或深色模式。</p>
                  <div className="theme-options">
                    {[
                      ['system', '跟随系统', '使用操作系统当前外观'],
                      ['light', '白天', '浅色界面，适合明亮环境'],
                      ['dark', '夜晚', '深色界面，降低眩光']
                    ].map(([value, label, description]) => (
                      <button
                        key={value}
                        className={`theme-option${(draft?.themeMode || 'system') === value ? ' active' : ''}`}
                        type="button"
                        onClick={() => setDraft((current) => ({ ...current, themeMode: value }))}
                      >
                        <span className={`theme-swatch ${value}`} />
                        <strong>{label}</strong>
                        <small>{description}</small>
                      </button>
                    ))}
                  </div>
                </div>
              )}
              {tab === 'servers' && (
                <>
                  <div className="settings-section-head">
                    <div>
                      <strong>上游服务器</strong>
                      <span>填写已经可以从本机访问的 Stellaclaw 地址；SSH 隧道请在外部建立后填本地转发地址。</span>
                    </div>
                    <button className="secondary-button" type="button" onClick={addServer}>添加</button>
                  </div>
                  <div className="server-editor-list">
                    {servers.map((server) => (
                      <article className="server-editor" key={server.id}>
                        <div className="server-editor-head">
                          <strong>{server.name || server.id}</strong>
                          <div>
                            <button
                              className="plain-button"
                              type="button"
                              onClick={() => setDraft((current) => ({ ...current, activeServerId: server.id }))}
                              disabled={draft?.activeServerId === server.id}
                            >
                              {draft?.activeServerId === server.id ? '当前服务器' : '设为当前'}
                            </button>
                            <button className="plain-danger-button" type="button" onClick={() => removeServer(server.id)} disabled={servers.length <= 1}>删除</button>
                          </div>
                        </div>
                        <div className="form-grid">
                          <label className="form-field">
                            <span>名称</span>
                            <input value={server.name || ''} onChange={(event) => updateServer(server.id, { name: event.target.value })} />
                          </label>
                          <label className="form-field">
                            <span>Base URL</span>
                            <input value={server.baseUrl || ''} onChange={(event) => updateServer(server.id, { baseUrl: event.target.value })} />
                          </label>
                          <label className="form-field wide">
                            <span>Token</span>
                            <input value={server.token || ''} onChange={(event) => updateServer(server.id, { token: event.target.value })} />
                          </label>
                        </div>
                      </article>
                    ))}
                  </div>
                </>
              )}
              {tab === 'update' && (
                <div className="settings-card">
                  <strong>更新</strong>
                  <p>当前版本 {appVersion || '未知'}。打包版本会在后台自动检查和下载更新；开发环境不会检查更新。下载完成后，标题栏右上角会出现 Update 按钮。</p>
                </div>
              )}
              {tab === 'about' && (
                <div className="settings-card">
                  <strong>Stellacode 2</strong>
                  <dl className="about-list">
                    <dt>版本</dt><dd>{appVersion || '未知'}</dd>
                    <dt>作者</dt><dd>{AUTHOR}</dd>
                    <dt>运行时</dt><dd>Electron · React</dd>
                  </dl>
                </div>
              )}
            </section>
          </div>
          <div className="dialog-actions">
            <Dialog.Close className="secondary-button" type="button">取消</Dialog.Close>
            <button className="primary-button" type="button" onClick={save} disabled={!draft || saving}>
              {saving ? '正在保存...' : '保存设置'}
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
