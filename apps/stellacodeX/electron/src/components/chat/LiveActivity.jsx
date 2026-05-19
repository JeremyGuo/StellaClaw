import { Pin } from 'lucide-react';

export function LiveActivityStack({ activities, progressRef }) {
  if (!activities?.length) return null;
  const current = activities.at(-1) || {};
  const plan = normalizeActivityPlan(current.plan);
  if (!plan) return null;
  return (
    <section className="session-progress-card with-plan" aria-live="polite" ref={progressRef}>
      <ActivityPlanPanel plan={plan} />
    </section>
  );
}

export function InlineActivityStatus({ activity }) {
  const state = String(activity?.state || 'running').toLowerCase();
  const title = String(activity?.title || '').trim();
  const detail = String(activity?.detail || activity?.activity || activity?.model || '').trim();
  const label = title || (state === 'failed' ? '执行失败' : state === 'done' ? '已完成' : '正在思考');
  return (
    <div className={`chat-activity-status ${state}`}>
      <i className="chat-activity-icon" aria-hidden="true" />
      <span>{label}</span>
      {detail && <code>{detail}</code>}
    </div>
  );
}

export function shouldShowInlineActivity(activity) {
  if (!activity) return false;
  const state = String(activity?.state || 'running').toLowerCase();
  if (state === 'failed') return true;
  return false;
}

function normalizeActivityPlan(rawPlan) {
  const items = Array.isArray(rawPlan)
    ? rawPlan
    : Array.isArray(rawPlan?.items)
      ? rawPlan.items
      : Array.isArray(rawPlan?.plan)
        ? rawPlan.plan
        : [];
  if (!items.length && !rawPlan?.explanation) return null;
  const normalized = items
    .map(normalizePlanItem)
    .filter((item) => item.step);
  const completed = normalized.filter((item) => item.status === 'completed' || item.status === 'done').length;
  return {
    explanation: String(rawPlan?.explanation || '').trim(),
    items: normalized,
    completed,
    total: normalized.length
  };
}

function normalizePlanItem(item) {
  const rawStep = String(item?.step || item?.text || item?.title || '').trim();
  const marker = rawStep.match(/^\[(x|~|\s*)]\s*/i)?.[1]?.toLowerCase();
  const markerStatus = marker === 'x'
    ? 'completed'
    : marker === '~'
      ? 'in_progress'
      : marker !== undefined
        ? 'pending'
        : '';
  const rawStatus = String(item?.status || item?.state || '').toLowerCase();
  const normalizedStatus = normalizePlanStatus(rawStatus);
  const status = markerStatus && (!normalizedStatus || normalizedStatus === 'pending')
    ? markerStatus
    : normalizedStatus || markerStatus || 'pending';
  return {
    step: rawStep.replace(/^\[(?:x|~|\s*)]\s*/i, '').trim(),
    status
  };
}

function normalizePlanStatus(status) {
  if (status === 'completed' || status === 'done' || status === 'success') return 'completed';
  if (status === 'in_progress' || status === 'running' || status === 'active') return 'in_progress';
  if (status === 'pending' || status === 'todo') return 'pending';
  return '';
}

function ActivityPlanPanel({ plan }) {
  return (
    <section className="activity-plan-panel">
      <div className="activity-plan-head">
        <span>进度</span>
        <code>共 {plan.total} 项，已完成 {plan.completed} 项</code>
        <Pin className="activity-plan-pin" size={15} aria-hidden="true" />
      </div>
      <div className="activity-plan-body">
        {plan.explanation && <p>{plan.explanation}</p>}
        {plan.items.map((item, index) => (
          <div className={`activity-plan-row ${planStatusClass(item.status)}`} key={`${item.step}-${index}`}>
            <span>{planStatusMark(item.status)}</span>
            <strong>{index + 1}.</strong>
            <em>{item.step}</em>
          </div>
        ))}
      </div>
    </section>
  );
}

function planStatusClass(status) {
  if (status === 'completed' || status === 'done') return 'completed';
  if (status === 'in_progress' || status === 'running') return 'running';
  return 'pending';
}

function planStatusMark(status) {
  if (status === 'completed' || status === 'done') return '✓';
  if (status === 'in_progress' || status === 'running') return '◌';
  return '○';
}
