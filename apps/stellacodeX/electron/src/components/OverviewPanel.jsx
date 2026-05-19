import { formatCompactNumber, formatCost } from '../lib/format';

export function OverviewPanel({ open, conversation, status, usage, title }) {
  const remoteLabel = conversation?.remote || status?.remote || '';
  const remote = isRemoteStatus(remoteLabel);
  const model = conversation?.model || status?.model || 'pending';
  const sandbox = conversation?.sandbox || status?.sandbox || 'pending';
  const memoryUsage = usageBucket(status?.usage?.memory);
  const userCompactionUsage = usageBucket(status?.usage?.user_memory_compaction);
  const memoryTotal = memoryUsage.totalTokens + userCompactionUsage.totalTokens;
  return (
    <aside className={`right-panel overview-panel${open ? ' open' : ''}`} aria-hidden={!open}>
      {open && (
        <>
          <header className="file-browser-header">
            <div>
              <strong>Conversation 概览</strong>
              <span>{conversation?.conversation_id || '未选择 Conversation'}</span>
            </div>
          </header>
          <div className="overview-panel-body">
            {!conversation ? (
              <div className="panel-placeholder">选择一个 Conversation 查看简介</div>
            ) : (
              <>
                <section className="overview-hero">
                  <span>{conversation.platform_chat_id || conversation.conversation_id}</span>
                  <strong>{title}</strong>
                  <p><i className={`status-dot${remote ? ' remote' : ''}`} />{remote ? remoteLabel : 'local workspace'}</p>
                </section>
                <section className="overview-metrics">
                  <div>
                    <span>Cache</span>
                    <strong>{Math.round((usage?.cacheHit || 0) * 100)}%</strong>
                  </div>
                  <div>
                    <span>Tokens</span>
                    <strong>{formatCompactNumber(usage?.totalTokens)}</strong>
                  </div>
                  <div>
                    <span>Cost</span>
                    <strong>{formatCost(usage?.cost)}</strong>
                  </div>
                </section>
                <section className="overview-card">
                  <h3>运行状态</h3>
                  <dl className="overview-kv">
                    <dt>model</dt><dd>{model}</dd>
                    <dt>sandbox</dt><dd>{sandbox}</dd>
                    <dt>background</dt><dd>{Number(status?.running_background || 0)} / {Number(status?.total_background || conversation?.total_background || 0)}</dd>
                    <dt>subagents</dt><dd>{Number(status?.running_subagents || 0)} / {Number(status?.total_subagents || conversation?.total_subagents || 0)}</dd>
                  </dl>
                </section>
                <section className="overview-card">
                  <h3>Usage</h3>
                  <UsageBar label="Cache Read" value={usage?.cacheRead} total={usage?.totalTokens} />
                  <UsageBar label="Cache Write" value={usage?.cacheWrite} total={usage?.totalTokens} />
                  <UsageBar label="Input" value={usage?.input} total={usage?.totalTokens} />
                  <UsageBar label="Output" value={usage?.output} total={usage?.totalTokens} />
                </section>
                <section className="overview-card">
                  <h3>Memory Usage</h3>
                  <div className="overview-metrics compact">
                    <div>
                      <span>Memory Cost</span>
                      <strong>{formatCost(memoryUsage.cost)}</strong>
                    </div>
                    <div>
                      <span>Compaction Cost</span>
                      <strong>{formatCost(userCompactionUsage.cost)}</strong>
                    </div>
                  </div>
                  <UsageBar label="Memory" value={memoryUsage.totalTokens} total={memoryTotal} />
                  <UsageBar label="User Compaction" value={userCompactionUsage.totalTokens} total={memoryTotal} />
                </section>
              </>
            )}
          </div>
        </>
      )}
    </aside>
  );
}

function usageBucket(bucket) {
  const cost = bucket?.cost || {};
  const cacheRead = Number(bucket?.cache_read || 0);
  const cacheWrite = Number(bucket?.cache_write || 0);
  const input = Number(bucket?.uncache_input || bucket?.input || 0);
  const output = Number(bucket?.output || 0);
  return {
    cacheRead,
    cacheWrite,
    input,
    output,
    totalTokens: cacheRead + cacheWrite + input + output,
    cost:
      Number(cost.cache_read || 0)
      + Number(cost.cache_write || 0)
      + Number(cost.uncache_input || cost.input || 0)
      + Number(cost.output || 0)
  };
}

function UsageBar({ label, value, total }) {
  const amount = Number(value || 0);
  const denominator = Number(total || 0);
  const percent = denominator > 0 ? Math.max(3, Math.min(100, Math.round((amount / denominator) * 100))) : 0;
  return (
    <div className="usage-row">
      <div className="usage-row-head"><span>{label}</span><strong>{formatCompactNumber(amount)}</strong></div>
      <div className="usage-track"><span style={{ width: `${percent}%` }} /></div>
    </div>
  );
}

function isRemoteStatus(remote) {
  if (!remote) return false;
  const normalized = String(remote).toLowerCase();
  return !['selectable', 'disabled', 'local', 'none'].includes(normalized);
}
