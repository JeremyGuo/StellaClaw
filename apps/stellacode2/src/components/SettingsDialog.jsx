import { useEffect, useMemo, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';

const AUTHOR = 'Stellacode contributors';

function updateStatusText(status) {
  const state = status?.state || 'idle';
  if (state === 'disabled') return '开发环境不会检查更新。打包版本才会连接 GitHub Release。';
  if (state === 'checking') return '正在检查更新...';
  if (state === 'downloading') {
    const percent = Number.isFinite(status?.percent) ? ` ${Math.round(status.percent)}%` : '';
    return `发现新版本，正在下载${percent}。`;
  }
  if (state === 'downloaded') {
    return `新版本 ${status.version || ''} 已下载，可以重启安装。`;
  }
  if (state === 'error') return `检查更新失败：${status.error || '未知错误'}`;
  return '当前没有已下载更新。打包版本会继续在后台自动检查。';
}

function blankServer(index) {
  return {
    id: `server-${index}`,
    name: `Server ${index}`,
    connectionMode: 'direct',
    baseUrl: 'http://127.0.0.1:3111',
    targetUrl: 'http://127.0.0.1:3111',
    sshHost: '',
    token: '',
    userName: 'workspace-user'
  };
}

export function SettingsDialog({ open, settings, saving, onOpenChange, onSave }) {
  const [tab, setTab] = useState('appearance');
  const [draft, setDraft] = useState(settings);
  const [appVersion, setAppVersion] = useState('');
  const [updaterStatus, setUpdaterStatus] = useState({ state: 'idle' });

  useEffect(() => {
    if (open) setDraft(settings);
  }, [open, settings]);

  useEffect(() => {
    if (!open) return;
    window.stellacode2?.appVersion?.()
      .then((version) => setAppVersion(version || ''))
      .catch(() => {});
  }, [open]);

  useEffect(() => {
    if (!open) return undefined;
    const updater = window.stellacode2?.updater;
    if (!updater) return undefined;
    let disposed = false;
    const applyStatus = (status) => {
      if (!disposed && status) setUpdaterStatus(status);
    };
    updater.status?.().then(applyStatus).catch(() => {});
    const unsubscribe = updater.onStatus?.(applyStatus);
    return () => {
      disposed = true;
      unsubscribe?.();
    };
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

  const checkForUpdate = () => {
    window.stellacode2?.updater?.check?.()
      .then((status) => {
        if (status) setUpdaterStatus(status);
      })
      .catch((error) => {
        setUpdaterStatus({ state: 'error', error: error?.message || String(error) });
      });
  };

  const installUpdate = () => {
    window.stellacode2?.updater?.install?.()
      .then((status) => {
        if (status) setUpdaterStatus(status);
      })
      .catch((error) => {
        setUpdaterStatus({ state: 'error', error: error?.message || String(error) });
      });
  };

  const updateBusy = updaterStatus?.state === 'checking' || updaterStatus?.state === 'downloading';
  const updateDisabled = updateBusy || updaterStatus?.state === 'disabled' || updaterStatus?.state === 'downloaded';

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
                      <span>Direct 直接访问 Base URL；SSH Proxy 会先连接 SSH Host 或 ~/.ssh/config alias，再从目标机器网络访问 Target URL。</span>
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
                            <span>连接模式</span>
                            <select
                              value={server.connectionMode || 'direct'}
                              onChange={(event) => updateServer(server.id, {
                                connectionMode: event.target.value,
                                targetUrl: server.targetUrl || server.baseUrl || 'http://127.0.0.1:3111'
                              })}
                            >
                              <option value="direct">Direct</option>
                              <option value="ssh_proxy">SSH Proxy</option>
                            </select>
                          </label>
                          {(server.connectionMode || 'direct') === 'ssh_proxy' ? (
                            <>
                              <label className="form-field">
                                <span>SSH Host / Alias</span>
                                <input value={server.sshHost || ''} onChange={(event) => updateServer(server.id, { sshHost: event.target.value })} placeholder="remote_server" />
                              </label>
                              <label className="form-field">
                                <span>Target URL</span>
                                <input value={server.targetUrl || server.baseUrl || ''} onChange={(event) => updateServer(server.id, { targetUrl: event.target.value })} placeholder="http://127.0.0.1:3111" />
                              </label>
                            </>
                          ) : (
                            <label className="form-field wide">
                              <span>Base URL</span>
                              <input value={server.baseUrl || ''} onChange={(event) => updateServer(server.id, { baseUrl: event.target.value })} />
                            </label>
                          )}
                          <label className="form-field">
                            <span>Username</span>
                            <input
                              value={server.userName || 'workspace-user'}
                              onChange={(event) => updateServer(server.id, { userName: event.target.value })}
                              placeholder="workspace-user"
                            />
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
                  <p>当前版本 {appVersion || '未知'}。打包版本会在后台自动检查和下载更新；也可以手动检查。下载完成后，标题栏右上角会出现 Update 按钮。</p>
                  <div className="update-settings-actions">
                    <button className="secondary-button" type="button" onClick={checkForUpdate} disabled={updateDisabled}>
                      {updateBusy ? '正在检查...' : '检查更新'}
                    </button>
                    {updaterStatus?.state === 'downloaded' && (
                      <button className="primary-button" type="button" onClick={installUpdate}>
                        重启并安装
                      </button>
                    )}
                  </div>
                  <p className="update-status-line">{updateStatusText(updaterStatus)}</p>
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
