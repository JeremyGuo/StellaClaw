const toolCategoryCache = new Map();

export function sameToolBlock(left, right) {
  if (left === right) return true;
  if (!left || !right) return false;
  if (
    left.id !== right.id
    || left.type !== right.type
    || left.kind !== right.kind
    || left.text !== right.text
  ) {
    return false;
  }
  return sameToolCards(left.cards, right.cards);
}

export function sameToolCards(leftCards = [], rightCards = []) {
  const left = Array.isArray(leftCards) ? leftCards : [];
  const right = Array.isArray(rightCards) ? rightCards : [];
  if (left.length !== right.length) return false;
  for (let index = 0; index < left.length; index += 1) {
    if (!sameToolCard(left[index], right[index])) return false;
  }
  return true;
}

function sameToolCard(left, right) {
  if (left === right) return true;
  if (!left || !right) return false;
  return left.renderId === right.renderId
    && left.id === right.id
    && left.kind === right.kind
    && left.name === right.name
    && left.payload === right.payload
    && left.sourceRowId === right.sourceRowId
    && sameUsage(left.sourceRowUsage, right.sourceRowUsage)
    && sameUsage(left.usage, right.usage);
}

export function mergedToolCards(cards) {
  const rows = [];
  const byId = new Map();
  cards.forEach((card, index) => {
    const id = String(card.id || '').trim();
    if (!id) {
      rows.push({ order: index, call: card.kind === 'call' ? card : null, result: card.kind === 'result' ? card : null, sourceRowIds: new Set([card.sourceRowId].filter(Boolean)) });
      return;
    }
    let row = byId.get(id);
    if (!row) {
      row = { order: index, call: null, result: null, sourceRowIds: new Set() };
      byId.set(id, row);
      rows.push(row);
    }
    if (card.sourceRowId) row.sourceRowIds.add(card.sourceRowId);
    if (card.kind === 'result') {
      row.result = card;
    } else {
      row.call = card;
    }
  });
  const orderedRows = rows.sort((left, right) => left.order - right.order);
  const usageBySourceRow = new Map();
  cards.forEach((card) => {
    if (card.sourceRowId && Number(card.sourceRowUsage?.total || 0)) {
      usageBySourceRow.set(card.sourceRowId, card.sourceRowUsage);
    }
  });
  const lastMergedRowBySource = new Map();
  orderedRows.forEach((row) => {
    row.sourceRowIds.forEach((sourceRowId) => {
      lastMergedRowBySource.set(sourceRowId, row);
    });
  });
  const usageByMergedRow = new Map();
  usageBySourceRow.forEach((usage, sourceRowId) => {
    const row = lastMergedRowBySource.get(sourceRowId);
    if (row) usageByMergedRow.set(row, addToolUsage(usageByMergedRow.get(row), usage));
  });
  return orderedRows.map((row, index) => {
    const call = row.call;
    const result = row.result;
    const displayCard = call || result;
    const detailCard = result || call;
    return {
      renderId: call?.renderId || result?.renderId || `tool-row-${index}`,
      id: displayCard?.id || detailCard?.id || '',
      kind: result ? 'result' : 'call',
      name: displayCard?.name || detailCard?.name || 'tool',
      payload: displayCard?.payload ?? detailCard?.payload ?? '',
      callPayload: call?.payload,
      resultPayload: result?.payload,
      usage: usageByMergedRow.get(row) || null,
      running: Boolean(call && !result)
    };
  });
}

function addToolUsage(left, right) {
  if (!Number(right?.total || 0)) return left || null;
  if (!left) {
    return {
      input: Number(right.input || 0),
      output: Number(right.output || 0),
      cacheRead: Number(right.cacheRead || 0),
      cacheWrite: Number(right.cacheWrite || 0),
      total: Number(right.total || 0)
    };
  }
  return {
    input: Number(left.input || 0) + Number(right.input || 0),
    output: Number(left.output || 0) + Number(right.output || 0),
    cacheRead: Number(left.cacheRead || 0) + Number(right.cacheRead || 0),
    cacheWrite: Number(left.cacheWrite || 0) + Number(right.cacheWrite || 0),
    total: Number(left.total || 0) + Number(right.total || 0)
  };
}

export function sameUsage(left, right) {
  if (left === right) return true;
  if (!left || !right) return !Number(left?.total || right?.total || 0);
  return Number(left.input || 0) === Number(right.input || 0)
    && Number(left.output || 0) === Number(right.output || 0)
    && Number(left.cacheRead || 0) === Number(right.cacheRead || 0)
    && Number(left.cacheWrite || 0) === Number(right.cacheWrite || 0)
    && Number(left.total || 0) === Number(right.total || 0);
}

export function toolCardsAreComplete(cards) {
  if (!cards.length) return false;
  const open = new Set();
  let hasResult = false;
  cards.forEach((card, index) => {
    const id = String(card.id || `${card.name || 'tool'}-${index}`);
    if (card.kind === 'call') {
      open.add(id);
    } else if (card.kind === 'result') {
      hasResult = true;
      open.delete(id);
    }
  });
  return hasResult && (open.size === 0 || cards.at(-1)?.kind === 'result');
}

export function toolGroupSummary(cards, fallbackName) {
  const records = toolOperationRecords(cards);
  const counts = new Map();
  records.forEach((card) => {
    const category = toolCategory(card.name);
    counts.set(category, (counts.get(category) || 0) + 1);
  });
  const parts = [
    toolCountLabel(counts, 'search'),
    toolCountLabel(counts, 'command'),
    toolCountLabel(counts, 'edit'),
    toolCountLabel(counts, 'fetch'),
    toolCountLabel(counts, 'read'),
    toolCountLabel(counts, 'image'),
    toolCountLabel(counts, 'plan'),
    toolCountLabel(counts, 'memory'),
    toolCountLabel(counts, 'skill'),
    toolCountLabel(counts, 'cron'),
    toolCountLabel(counts, 'agent'),
    toolCountLabel(counts, 'tool')
  ].filter(Boolean);
  const doneTitle = parts.join(' ') || `已处理 ${fallbackName || '工具'}`;
  const runningTitle = `正在处理 ${fallbackName || '工具'}`;
  return { doneTitle, runningTitle };
}

function toolOperationRecords(cards) {
  const hasCalls = cards.some((card) => card.kind === 'call');
  const seen = new Set();
  return cards.filter((card, index) => {
    if (hasCalls && card.kind !== 'call') return false;
    const id = String(card.id || '').trim();
    const key = id || `${card.kind}:${card.name || 'tool'}:${index}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function toolCategory(name) {
  const lowerName = String(name || '').toLowerCase();
  const cached = toolCategoryCache.get(lowerName);
  if (cached) return cached;
  let category = 'tool';
  if (lowerName.includes('search') || lowerName.includes('grep') || lowerName === 'rg') category = 'search';
  else if (lowerName.includes('shell') || lowerName.includes('command') || lowerName.includes('terminal') || lowerName.includes('stdin')) category = 'command';
  else if (lowerName.includes('edit') || lowerName.includes('write') || lowerName.includes('patch')) category = 'edit';
  else if (lowerName.includes('fetch') || lowerName.includes('browser') || lowerName.includes('open_url')) category = 'fetch';
  else if (lowerName.includes('file_read') || lowerName.includes('read')) category = 'read';
  else if (lowerName.includes('image') || lowerName.includes('screenshot')) category = 'image';
  else if (lowerName.includes('plan')) category = 'plan';
  else if (lowerName.includes('memory')) category = 'memory';
  else if (lowerName.includes('skill')) category = 'skill';
  else if (lowerName.includes('cron') || lowerName.includes('automation')) category = 'cron';
  else if (lowerName.includes('agent') || lowerName.includes('subagent')) category = 'agent';
  toolCategoryCache.set(lowerName, category);
  return category;
}

function toolCountLabel(counts, category) {
  const count = counts.get(category) || 0;
  if (!count) return '';
  if (category === 'search') return `已探索 ${count} 次搜索`;
  if (category === 'command') return `已运行 ${count} 条命令`;
  if (category === 'edit') return `已编辑 ${count} 次`;
  if (category === 'fetch') return `已抓取 ${count} 个网页`;
  if (category === 'read') return `已读取 ${count} 个文件`;
  if (category === 'image') return `已查看 ${count} 张图片`;
  if (category === 'plan') return count === 1 ? '已更新计划' : `已更新 ${count} 次计划`;
  if (category === 'memory') return `已访问 ${count} 次记忆`;
  if (category === 'skill') return `已运行 ${count} 个技能`;
  if (category === 'cron') return `已处理 ${count} 个定时任务`;
  if (category === 'agent') return `已运行 ${count} 个代理`;
  return `已调用 ${count} 个工具`;
}
