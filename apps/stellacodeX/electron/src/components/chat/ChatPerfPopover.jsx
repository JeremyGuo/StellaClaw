import { useEffect, useState } from 'react';
import { Info } from 'lucide-react';
import * as Popover from '@radix-ui/react-popover';
import { clearChatPerf, setChatPerfDetailedEnabled, snapshotChatPerf, startChatFrameProbe } from '../../lib/chatPerfMetrics';

export function ChatPerfPopover() {
  const [open, setOpen] = useState(false);
  const [snapshot, setSnapshot] = useState(() => snapshotChatPerf());
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    setChatPerfDetailedEnabled(open);
    if (!open) return () => setChatPerfDetailedEnabled(false);
    setSnapshot(snapshotChatPerf());
    const stopProbe = startChatFrameProbe(true);
    const timer = window.setInterval(() => {
      setSnapshot(snapshotChatPerf());
    }, 600);
    return () => {
      setChatPerfDetailedEnabled(false);
      stopProbe?.();
      window.clearInterval(timer);
    };
  }, [open]);
  const rows = snapshot.rows || [];
  const total = rows.reduce((sum, row) => sum + Number(row.totalMs || 0), 0);
  const copySnapshot = async () => {
    try {
      await navigator.clipboard?.writeText(JSON.stringify(snapshotChatPerf(), null, 2));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      setCopied(false);
    }
  };
  const reset = () => {
    clearChatPerf();
    setSnapshot(snapshotChatPerf());
  };
  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button className="chat-perf-trigger" type="button" title="查看 Chat 性能统计" aria-label="查看 Chat 性能统计">
          <Info size={15} strokeWidth={2} aria-hidden="true" />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="chat-perf-popover" side="top" align="start" sideOffset={10}>
          <div className="chat-perf-header">
            <div>
              <strong>Chat 性能</strong>
              <span>{snapshot.capturedAt}</span>
            </div>
            <em>{rows.length} 项 · {formatPerfMs(total)}</em>
          </div>
          <div className="chat-perf-actions">
            <button type="button" onClick={copySnapshot}>{copied ? '已复制' : '复制'}</button>
            <button type="button" onClick={reset}>清空</button>
          </div>
          <div className="chat-perf-table" role="table" aria-label="Chat performance metrics">
            <div className="chat-perf-row head" role="row">
              <span>Metric</span>
              <span>Last</span>
              <span>Avg</span>
              <span>Max</span>
              <span>Count</span>
            </div>
            {rows.slice(0, 20).map((row) => (
              <div className="chat-perf-row" role="row" key={row.name} title={perfMetaTitle(row.lastMeta)}>
                <span>{row.name}</span>
                <span>{formatPerfMs(row.lastMs)}</span>
                <span>{formatPerfMs(row.avgMsRounded)}</span>
                <span>{formatPerfMs(row.maxMs)}</span>
                <span>{row.count}</span>
              </div>
            ))}
            {rows.length === 0 && <div className="chat-perf-empty">暂无采样；保持面板打开并复现卡顿。</div>}
          </div>
          <div className="chat-perf-events">
            <strong>慢事件</strong>
            {(snapshot.events || []).slice(0, 10).map((event, index) => (
              <div className="chat-perf-event" key={`${event.time}-${event.name}-${index}`} title={perfMetaTitle(event.meta)}>
                <span>{event.time}</span>
                <em>{event.name}</em>
                <b>{formatPerfMs(event.durationMs)}</b>
              </div>
            ))}
            {(snapshot.events || []).length === 0 && <div className="chat-perf-empty">暂无慢事件</div>}
          </div>
          <Popover.Arrow className="floating-popover-arrow" />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

function formatPerfMs(value) {
  const number = Number(value || 0);
  if (number >= 1000) return `${(number / 1000).toFixed(1)}s`;
  if (number >= 100) return `${Math.round(number)}ms`;
  return `${number.toFixed(number >= 10 ? 1 : 2)}ms`;
}

function perfMetaTitle(meta) {
  if (!meta) return '';
  try {
    return JSON.stringify(meta, null, 2);
  } catch {
    return String(meta);
  }
}
