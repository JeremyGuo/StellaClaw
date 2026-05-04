import { useEffect, useMemo, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';

const AUTHOR = 'Stellacode contributors';
const MIN_DISPLAY_FONT_SIZE = 11;
const MAX_DISPLAY_FONT_SIZE = 18;
const DEFAULT_DISPLAY_FONT_SIZE = 12;
const MIN_UI_SCALE = 0.8;
const MAX_UI_SCALE = 1.4;
const DEFAULT_UI_SCALE = 1;

function normalizeDisplayFontSize(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return DEFAULT_DISPLAY_FONT_SIZE;
  return Math.min(MAX_DISPLAY_FONT_SIZE, Math.max(MIN_DISPLAY_FONT_SIZE, Math.round(number)));
}

function normalizeUiScale(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return DEFAULT_UI_SCALE;
  return Math.min(MAX_UI_SCALE, Math.max(MIN_UI_SCALE, Math.round(number * 20) / 20));
}

function githubUpdateStatusText(status) {
  const state = status?.state || 'idle';
  if (state === 'disabled') return '开发环境不会检查 GitHub 更新。打包版本才会连接 GitHub Release。';
  if (state === 'checking') return '正在检查 GitHub 更新...';
  if (state === 'downloading') {
    const percent = Number.isFinite(status?.percent) ? ` ${Math.round(status.percent)}%` : '';
    return `发现 GitHub 新版本，正在下载${percent}。`;
  }
  if (state === 'downloaded') return `GitHub 新版本 ${status.version || ''} 已下载，可以重启安装。`;
  if (state === 'error') return `GitHub 更新失败：${status.error || '未知错误'}`;
  return 'GitHub 通道会在打包版本后台自动检查和下载更新，也可以手动检查。';
}

function updateStatusText(status) {
  const state = status?.state || 'idle';
  const channel = status?.channel || 'stable';
  if (state === 'checking') return `正在检查 ${channel} 更新...`;
  if (state === 'downloading') return status.message || `正在通过 SSH 下载 ${channel} 安装包...`;
  if (state === 'downloaded') return status.message || `${channel} 安装包已下载。`;
  if (state === 'error') return `更新失败：${status.error || '未知错误'}`;
  if (status?.message) return status.message;
  return '通过当前 SSH Profile 从固定路径拉取 test 或 stable 安装包。';
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
  const [githubUpdaterStatus, setGithubUpdaterStatus] = useState({ state: 'idle' });
  const [sshUpdaterStatus, setSshUpdaterStatus] = useState({ state: 'idle', channel: 'stable' });

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
    const sshUpdater = window.stellacode2?.sshUpdater;
    let disposed = false;
    const applyGithubStatus = (status) => {
      if (!disposed && status) setGithubUpdaterStatus(status);
    };
    const applySshStatus = (status) => {
      if (!disposed && status) setSshUpdaterStatus(status);
    };
    updater?.status?.().then(applyGithubStatus).catch(() => {});
    sshUpdater?.status?.().then(applySshStatus).catch(() => {});
    const unsubscribeGithub = updater?.onStatus?.(applyGithubStatus);
    const unsubscribeSsh = sshUpdater?.onStatus?.(applySshStatus);
    return () => {
      disposed = true;
      unsubscribeGithub?.();
      unsubscribeSsh?.();
    };
  }, [open]);

  const servers = useMemo(() => draft?.servers || [], [draft]);
  const displayFontSize = normalizeDisplayFontSize(draft?.displayFontSize);
  const uiScale = normalizeUiScale(draft?.uiScale);

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

  const checkGithubUpdate = () => {
    window.stellacode2?.updater?.check?.()
      .then((status) => {
        if (status) setGithubUpdaterStatus(status);
      })
      .catch((error) => {
        setGithubUpdaterStatus({ state: 'error', error: error?.message || String(error) });
      });
  };

  const installGithubUpdate = () => {
    window.stellacode2?.updater?.install?.()
      .then((status) => {
        if (status) setGithubUpdaterStatus(status);
      })
      .catch((error) => {
        setGithubUpdaterStatus({ state: 'error', error: error?.message || String(error) });
      });
  };

  const checkForUpdate = (channel = 'stable') => {
    window.stellacode2?.sshUpdater?.check?.(channel)
      .then((status) => {
        if (status) setSshUpdaterStatus(status);
      })
      .catch((error) => {
        setSshUpdaterStatus({ state: 'error', error: error?.message || String(error), channel });
      });
  };

  const installUpdate = (channel = 'stable') => {
    window.stellacode2?.sshUpdater?.install?.(channel)
      .then((status) => {
        if (status) setSshUpdaterStatus(status);
      })
      .catch((error) => {
        setSshUpdaterStatus({ state: 'error', error: error?.message || String(error), channel });
      });
  };

  const installTest = () => installUpdate('test');
  const installStable = () => installUpdate('stable');

  const githubBusy = githubUpdaterStatus?.state === 'checking' || githubUpdaterStatus?.state === 'downloading';
  const githubDisabled = githubBusy || githubUpdaterStatus?.state === 'disabled' || githubUpdaterStatus?.state === 'downloaded';
  const updateBusy = sshUpdaterStatus?.state === 'checking' || sshUpdaterStatus?.state === 'downloading';
  const updateDisabled = updateBusy;

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
                  <div className="font-size-control">
                    <div className="font-size-control-head">
                      <div>
                        <strong>整体缩放</strong>
                        <span>按系统缩放之上再调整整个客户端 UI，适合高 DPI 或远程桌面环境。</span>
                      </div>
                      <em>{Math.round(uiScale * 100)}%</em>
                    </div>
                    <input
                      type="range"
                      min={MIN_UI_SCALE}
                      max={MAX_UI_SCALE}
                      step="0.05"
                      value={uiScale}
                      onChange={(event) => {
                        const nextScale = normalizeUiScale(event.target.value);
                        setDraft((current) => ({ ...current, uiScale: nextScale }));
                      }}
                    />
                    <div className="font-size-presets" aria-label="整体缩放快捷选项">
                      {[
                        [0.85, '85%'],
                        [1, '默认'],
                        [1.1, '110%'],
                        [1.25, '125%'],
                        [1.4, '140%']
                      ].map(([value, label]) => (
                        <button
                          key={value}
                          className={uiScale === value ? 'active' : ''}
                          type="button"
                          onClick={() => setDraft((current) => ({ ...current, uiScale: value }))}
                        >
                          {label}
                        </button>
                      ))}
                    </div>
                  </div>
                  <div className="font-size-control">
                    <div className="font-size-control-head">
                      <div>
                        <strong>显示字号</strong>
                        <span>调节客户端界面和聊天内容的基础字号。</span>
                      </div>
                      <em>{displayFontSize}px</em>
                    </div>
                    <input
                      type="range"
                      min={MIN_DISPLAY_FONT_SIZE}
                      max={MAX_DISPLAY_FONT_SIZE}
                      step="1"
                      value={displayFontSize}
                      onChange={(event) => {
                        const nextSize = normalizeDisplayFontSize(event.target.value);
                        setDraft((current) => ({ ...current, displayFontSize: nextSize }));
                      }}
                    />
                    <div className="font-size-presets" aria-label="字号快捷选项">
                      {[
                        [11, '小'],
                        [12, '默认'],
                        [14, '舒适'],
                        [16, '大'],
                        [18, '特大']
                      ].map(([value, label]) => (
                        <button
                          key={value}
                          className={displayFontSize === value ? 'active' : ''}
                          type="button"
                          onClick={() => setDraft((current) => ({ ...current, displayFontSize: value }))}
                        >
                          {label}
                        </button>
                      ))}
                    </div>
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
                  <p>当前版本 {appVersion || '未知'}。优先保留 GitHub Release 自动更新；也可以通过当前 SSH Profile 从固定路径拉取 test 或 stable 安装包。</p>
                  <div className="update-settings-actions">
                    <button className="secondary-button" type="button" onClick={checkGithubUpdate} disabled={githubDisabled}>
                      {githubBusy ? 'GitHub 检查中...' : '检查 GitHub 更新'}
                    </button>
                    {githubUpdaterStatus?.state === 'downloaded' && (
                      <button className="primary-button" type="button" onClick={installGithubUpdate}>
                        重启并安装 GitHub 更新
                      </button>
                    )}
                  </div>
                  <p className="update-status-line">{githubUpdateStatusText(githubUpdaterStatus)}</p>
                  <div className="update-settings-actions">
                    <button className="secondary-button" type="button" onClick={() => checkForUpdate('test')} disabled={updateDisabled}>
                      检查 SSH test
                    </button>
                    <button className="secondary-button" type="button" onClick={() => checkForUpdate('stable')} disabled={updateDisabled}>
                      检查 SSH stable
                    </button>
                    <button className="secondary-button" type="button" onClick={installTest} disabled={updateDisabled}>
                      安装 latest SSH test
                    </button>
                    <button className="primary-button" type="button" onClick={installStable} disabled={updateDisabled}>
                      安装 latest SSH stable
                    </button>
                  </div>
                  {sshUpdaterStatus?.remotePath && <p className="update-status-line">SSH 远程路径：{sshUpdaterStatus.remotePath}</p>}
                  <p className="update-status-line">{updateStatusText(sshUpdaterStatus)}</p>
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
