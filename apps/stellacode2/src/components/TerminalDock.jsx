import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { FitAddon } from '@xterm/addon-fit';
import { Terminal } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import { Plus, RefreshCw, TerminalSquare, X } from 'lucide-react';
import { createTerminal, listTerminals, terminalStreamUrl, terminateTerminal } from '../lib/api';

const DEFAULT_COLS = 120;
const DEFAULT_ROWS = 30;
const RECONNECT_DELAY_MS = 450;
const textEncoder = new TextEncoder();

function terminalLabel(terminal, index) {
  const numeric = String(terminal?.terminal_id || '').match(/(\d+)$/)?.[1];
  const suffix = numeric ? String(Number(numeric)) : String(index + 1);
  return `Terminal ${suffix}`;
}

function sendJson(socket, payload) {
  if (socket?.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(payload));
    return true;
  }
  return false;
}

export function TerminalDock({ open, serverId, conversationId, onResizeHeight, onResizeList }) {
  const [terminals, setTerminals] = useState([]);
  const [activeTerminalId, setActiveTerminalId] = useState('');
  const [loading, setLoading] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState('');
  const [connected, setConnected] = useState(false);
  const terminalHostRef = useRef(null);
  const terminalRef = useRef(null);
  const fitAddonRef = useRef(null);
  const socketRef = useRef(null);
  const offsetRef = useRef(0);
  const desiredSizeRef = useRef({ cols: DEFAULT_COLS, rows: DEFAULT_ROWS });
  const inputQueueRef = useRef([]);
  const loadSeqRef = useRef(0);
  const activeRunningRef = useRef(true);

  const activeTerminal = useMemo(
    () => terminals.find((terminal) => terminal.terminal_id === activeTerminalId) || null,
    [terminals, activeTerminalId]
  );

  useEffect(() => {
    activeRunningRef.current = activeTerminal?.running !== false;
  }, [activeTerminal?.running]);

  const refreshTerminals = useCallback(async (options = {}) => {
    if (!serverId || !conversationId) return [];
    const seq = ++loadSeqRef.current;
    if (!options.silent) setLoading(true);
    setError('');
    try {
      let next = await listTerminals(serverId, conversationId);
      if (!next.length && options.ensure) {
        const size = desiredSizeRef.current;
        const created = await createTerminal(serverId, conversationId, size);
        next = [created];
      }
      if (seq !== loadSeqRef.current) return next;
      setTerminals(next);
      setActiveTerminalId((current) => {
        if (current && next.some((terminal) => terminal.terminal_id === current)) return current;
        return next[0]?.terminal_id || '';
      });
      return next;
    } catch (loadError) {
      if (seq === loadSeqRef.current) {
        setError(loadError?.message || '读取终端失败');
      }
      return [];
    } finally {
      if (seq === loadSeqRef.current && !options.silent) setLoading(false);
    }
  }, [serverId, conversationId]);

  const updateTerminal = useCallback((terminalId, patch) => {
    setTerminals((current) => current.map((terminal) => (
      terminal.terminal_id === terminalId ? { ...terminal, ...patch } : terminal
    )));
  }, []);

  const createNewTerminal = useCallback(async () => {
    if (!serverId || !conversationId || creating) return;
    setCreating(true);
    setError('');
    try {
      const created = await createTerminal(serverId, conversationId, desiredSizeRef.current);
      setTerminals((current) => [...current, created]);
      setActiveTerminalId(created.terminal_id);
    } catch (createError) {
      setError(createError?.message || '创建终端失败');
    } finally {
      setCreating(false);
    }
  }, [serverId, conversationId, creating]);

  const closeTerminal = useCallback(async (terminalId) => {
    if (!serverId || !conversationId || !terminalId) return;
    setError('');
    try {
      await terminateTerminal(serverId, conversationId, terminalId);
      setTerminals((current) => {
        const closingIndex = current.findIndex((terminal) => terminal.terminal_id === terminalId);
        const next = current.filter((terminal) => terminal.terminal_id !== terminalId);
        setActiveTerminalId((selected) => {
          if (selected !== terminalId) return selected;
          return next[closingIndex]?.terminal_id || next[closingIndex - 1]?.terminal_id || next[0]?.terminal_id || '';
        });
        return next;
      });
    } catch (closeError) {
      setError(closeError?.message || '关闭终端失败');
    }
  }, [serverId, conversationId]);

  const flushInputQueue = useCallback(() => {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN || !inputQueueRef.current.length) return;
    for (const chunk of inputQueueRef.current.splice(0)) {
      socket.send(chunk);
    }
  }, []);

  const sendInput = useCallback((data) => {
    const bytes = textEncoder.encode(data);
    const socket = socketRef.current;
    if (socket?.readyState === WebSocket.OPEN) {
      socket.send(bytes);
    } else {
      inputQueueRef.current.push(bytes);
    }
  }, []);

  const sendResize = useCallback((cols, rows) => {
    desiredSizeRef.current = { cols, rows };
    sendJson(socketRef.current, { type: 'resize', cols, rows });
  }, []);

  useEffect(() => {
    if (!open || !serverId || !conversationId) {
      setTerminals([]);
      setActiveTerminalId('');
      setError('');
      setConnected(false);
      return;
    }
    refreshTerminals({ ensure: true }).catch(() => {});
  }, [open, serverId, conversationId, refreshTerminals]);

  useEffect(() => {
    if (!open || !activeTerminalId || !terminalHostRef.current) return undefined;
    const terminal = new Terminal({
      allowProposedApi: false,
      convertEol: false,
      cursorBlink: true,
      cursorStyle: 'block',
      fontFamily: '"SFMono-Regular", Menlo, Monaco, Consolas, "Liberation Mono", monospace',
      fontSize: 13,
      lineHeight: 1.12,
      scrollback: 12000,
      theme: {
        background: '#080a09',
        foreground: '#d9e4de',
        cursor: '#d9e4de',
        selectionBackground: '#2d5c4f',
        black: '#151918',
        red: '#f87171',
        green: '#5ee0a1',
        yellow: '#f4d35e',
        blue: '#60a5fa',
        magenta: '#c084fc',
        cyan: '#67e8f9',
        white: '#e5e7eb',
        brightBlack: '#64706a',
        brightRed: '#fca5a5',
        brightGreen: '#86efac',
        brightYellow: '#fde68a',
        brightBlue: '#93c5fd',
        brightMagenta: '#d8b4fe',
        brightCyan: '#a5f3fc',
        brightWhite: '#ffffff'
      }
    });
    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.open(terminalHostRef.current);
    terminal.focus();
    terminalRef.current = terminal;
    fitAddonRef.current = fitAddon;
    offsetRef.current = 0;
    inputQueueRef.current = [];
    const inputDisposable = terminal.onData(sendInput);
    const fit = () => {
      try {
        fitAddon.fit();
        const { cols, rows } = terminal;
        if (cols && rows) sendResize(cols, rows);
      } catch {
        // The terminal can be measured before layout settles; the next resize will retry.
      }
    };
    const resizeObserver = new ResizeObserver(fit);
    resizeObserver.observe(terminalHostRef.current);
    requestAnimationFrame(fit);
    return () => {
      resizeObserver.disconnect();
      inputDisposable.dispose();
      terminal.dispose();
      if (terminalRef.current === terminal) terminalRef.current = null;
      if (fitAddonRef.current === fitAddon) fitAddonRef.current = null;
    };
  }, [open, activeTerminalId, sendInput, sendResize]);

  useEffect(() => {
    if (!open || !serverId || !conversationId || !activeTerminalId || !terminalRef.current) {
      socketRef.current?.close();
      socketRef.current = null;
      setConnected(false);
      return undefined;
    }

    let disposed = false;
    let reconnectTimer = null;

    const connect = async () => {
      try {
        const url = await terminalStreamUrl(serverId, conversationId, activeTerminalId, offsetRef.current);
        if (disposed) return;
        const socket = new WebSocket(url);
        socket.binaryType = 'arraybuffer';
        socketRef.current = socket;
        let opened = false;
        socket.addEventListener('open', () => {
          if (disposed || socketRef.current !== socket) return;
          opened = true;
          setConnected(true);
          setError('');
          flushInputQueue();
          const { cols, rows } = desiredSizeRef.current;
          sendJson(socket, { type: 'resize', cols, rows });
        });
        socket.addEventListener('message', (event) => {
          if (disposed || socketRef.current !== socket) return;
          if (typeof event.data === 'string') {
            try {
              const message = JSON.parse(event.data);
              if (message.type === 'attached') {
                setError('');
                offsetRef.current = Number(message.replay_start_offset ?? message.next_offset ?? offsetRef.current) || 0;
                activeRunningRef.current = Boolean(message.running);
                updateTerminal(activeTerminalId, { running: Boolean(message.running) });
              } else if (message.type === 'dropped') {
                offsetRef.current = Math.max(offsetRef.current, Number(message.buffer_start_offset) || 0);
                terminalRef.current?.writeln('\r\n[terminal output buffer dropped older bytes]\r\n');
              } else if (message.type === 'exit') {
                activeRunningRef.current = false;
                updateTerminal(activeTerminalId, { running: false });
                setConnected(false);
              } else if (message.type === 'detached') {
                setConnected(false);
                if (!disposed) reconnectTimer = window.setTimeout(connect, RECONNECT_DELAY_MS);
              } else if (message.type === 'error') {
                setError(message.message || message.error || '终端连接错误');
              }
            } catch {
              setError('终端控制消息解析失败');
            }
            return;
          }
          const bytes = event.data instanceof ArrayBuffer
            ? new Uint8Array(event.data)
            : new Uint8Array(event.data || []);
          if (!bytes.length) return;
          offsetRef.current += bytes.byteLength;
          terminalRef.current?.write(bytes);
        });
        socket.addEventListener('close', () => {
          if (socketRef.current === socket) socketRef.current = null;
          setConnected(false);
          if (!disposed && !opened) {
            setError('终端实时连接未建立，请确认 Stellaclaw 已更新并重启');
          }
          if (!disposed && activeRunningRef.current !== false) {
            reconnectTimer = window.setTimeout(connect, RECONNECT_DELAY_MS);
          }
        });
        socket.addEventListener('error', () => {
          if (!disposed && !opened) {
            setError('终端实时连接异常，请确认后端 terminal stream 可用');
          }
        });
      } catch (connectError) {
        if (!disposed) {
          setError(connectError?.message || '终端实时连接失败');
          reconnectTimer = window.setTimeout(connect, RECONNECT_DELAY_MS * 2);
        }
      }
    };

    connect();
    return () => {
      disposed = true;
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      if (socketRef.current) {
        socketRef.current.close();
        socketRef.current = null;
      }
      setConnected(false);
    };
  }, [open, serverId, conversationId, activeTerminalId, flushInputQueue, updateTerminal]);

  if (!open) return null;

  return (
    <section className="terminal-dock">
      <button
        className="terminal-height-handle"
        type="button"
        aria-label="调整终端高度"
        onPointerDown={onResizeHeight}
      />
      <div className="terminal-dock-body">
        <aside className="terminal-session-list">
          <div className="terminal-list-header">
            <div className="terminal-title" title={connected ? '实时连接' : loading ? '正在连接' : '未连接'}>
              <TerminalSquare size={15} />
              <strong>Terminal</strong>
            </div>
            <div className="terminal-actions">
              <button className="terminal-icon-button" type="button" onClick={() => refreshTerminals({ silent: true })} title="刷新终端">
                <RefreshCw size={14} />
              </button>
              <button className="terminal-icon-button" type="button" onClick={createNewTerminal} disabled={creating || !serverId || !conversationId} title="新建终端">
                <Plus size={15} />
              </button>
            </div>
          </div>
          {terminals.map((terminal, index) => (
            <button
              key={terminal.terminal_id}
              className={`terminal-session${terminal.terminal_id === activeTerminalId ? ' active' : ''}`}
              type="button"
              onClick={() => setActiveTerminalId(terminal.terminal_id)}
            >
              <span className="terminal-session-main">
                <strong>{terminalLabel(terminal, index)}</strong>
              </span>
              <span
                className="terminal-session-close"
                role="button"
                tabIndex={0}
                title="关闭终端"
                onClick={(event) => {
                  event.stopPropagation();
                  closeTerminal(terminal.terminal_id);
                }}
                onKeyDown={(event) => {
                  if (event.key === 'Enter' || event.key === ' ') {
                    event.preventDefault();
                    event.stopPropagation();
                    closeTerminal(terminal.terminal_id);
                  }
                }}
              >
                <X size={13} />
              </span>
            </button>
          ))}
          {!terminals.length && (
            <div className="terminal-empty">{loading ? '正在创建终端...' : '暂无终端'}</div>
          )}
        </aside>
        <button
          className="terminal-list-handle"
          type="button"
          aria-label="调整终端会话列表宽度"
          onPointerDown={onResizeList}
        />
        <main className="terminal-stage">
          {error && <div className="terminal-error">{error}</div>}
          {!serverId || !conversationId ? (
            <div className="terminal-placeholder">选择一个 Conversation 后使用终端</div>
          ) : (
            <div ref={terminalHostRef} className="terminal-xterm" />
          )}
        </main>
      </div>
    </section>
  );
}
