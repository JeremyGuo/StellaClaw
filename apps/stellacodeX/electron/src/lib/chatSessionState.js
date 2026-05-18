import { messageOrderFromId } from './messageUtils';

export function recentMessagePageParams(conversation, limit = 40, totalOverride = undefined) {
  const boundedLimit = Math.max(1, Math.min(200, Number(limit) || 40));
  const overrideTotal = Number(totalOverride);
  const lastId = messageOrderFromId(conversation?.last_message_id);
  const messageCount = Number(conversation?.message_count);
  let total = 0;
  if (Number.isFinite(overrideTotal) && overrideTotal > 0) {
    total = overrideTotal;
  } else if (Number.isFinite(messageCount) && messageCount > 0) {
    total = messageCount;
  } else if (Number.isFinite(lastId) && lastId >= 0) {
    total = lastId + 1;
  }
  return {
    offset: Math.max(0, total - boundedLimit),
    limit: boundedLimit
  };
}

export function isActiveSessionState(value) {
  return ['running', 'queued', 'processing', 'in_progress', 'active'].includes(
    String(value || '').trim().toLowerCase()
  );
}

export function chatSnapshotState(snapshot) {
  const currentTurnState = snapshot?.current_turn_state || snapshot?.currentTurnState || null;
  const queued = Array.isArray(snapshot?.queued_outbound_messages)
    ? snapshot.queued_outbound_messages
    : Array.isArray(snapshot?.queuedOutboundMessages)
      ? snapshot.queuedOutboundMessages
      : [];
  const state = currentTurnState
    ? 'running'
    : queued.length > 0
      ? 'queued'
      : 'idle';
  return {
    state,
    currentTurnState,
    activeTurnId: String(currentTurnState?.turn_id || currentTurnState?.turnId || '').trim()
  };
}

export function chatSessionStateIsActive(state) {
  return isActiveSessionState(state?.state);
}

function normalizePlan(rawPlan) {
  const items = Array.isArray(rawPlan)
    ? rawPlan
    : Array.isArray(rawPlan?.plan)
      ? rawPlan.plan
      : Array.isArray(rawPlan?.items)
        ? rawPlan.items
        : [];
  const plan = items
    .map(normalizePlanItem)
    .filter((item) => item.step);
  const explanation = String(rawPlan?.explanation || rawPlan?.summary || '').trim();
  return (explanation || plan.length) ? { explanation, plan } : null;
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
  const rawStatus = normalizePlanStatus(String(item?.status || item?.state || '').trim().toLowerCase());
  const status = markerStatus && (!rawStatus || rawStatus === 'pending')
    ? markerStatus
    : rawStatus || markerStatus || 'pending';
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

export function normalizeProgressFeedback(payload) {
  const source = payload.progress || payload.event || payload;
  const finalState = source.final_state || source.finalState || payload.final_state || payload.finalState || '';
  const phase = source.phase || payload.phase || '';
  const state = finalState === 'failed' || phase === 'failed'
    ? 'failed'
    : finalState === 'done' || phase === 'done'
      ? 'done'
      : 'running';
  const plan = normalizePlan(source.plan || source.task_plan || source.taskPlan || payload.plan);
  const activity = String(
    source.activity
    || source.stage
    || source.phase
    || source.status
    || (state === 'done' ? '已完成' : state === 'failed' ? '执行失败' : '思考中')
  ).trim();
  const model = String(source.model || source.model_name || source.modelName || '').trim();
  return {
    id: `progress-${source.turn_id || source.turnId || payload.turn_id || 'current'}`,
    title: state === 'failed' ? '执行失败' : state === 'done' ? '执行完毕' : '思考中',
    detail: activity,
    activity,
    model,
    plan,
    state
  };
}

export function mergeProgressActivity(current, progress) {
  const existing = current.find((item) => item.id === progress.id);
  return {
    ...existing,
    ...progress,
    plan: progress.plan || existing?.plan || null,
    model: progress.model || existing?.model || ''
  };
}
