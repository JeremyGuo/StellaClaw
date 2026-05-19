import { chatRenderEntryKey } from './renderModel';

export const VIRTUALIZE_ENTRY_THRESHOLD = 80;
const VIRTUAL_ENTRY_ESTIMATE = 150;
const VIRTUAL_OVERSCAN_PX = 1100;

export function virtualWindowForEntries({ entries, keys, heightCache, viewport, activeIndex }) {
  const count = entries.length;
  if (count <= VIRTUALIZE_ENTRY_THRESHOLD) {
    return {
      virtualized: false,
      start: 0,
      end: Math.max(0, count - 1),
      topPadding: 0,
      bottomPadding: 0,
      items: entries.map((entry, index) => ({ entry, index, key: keys[index] || chatRenderEntryKey(entry, index) }))
    };
  }
  const heights = keys.map((key) => heightCache.get(key) || VIRTUAL_ENTRY_ESTIMATE);
  const offsets = new Array(count + 1);
  offsets[0] = 0;
  for (let index = 0; index < count; index += 1) {
    offsets[index + 1] = offsets[index] + heights[index];
  }
  const top = Math.max(0, Number(viewport.scrollTop || 0) - VIRTUAL_OVERSCAN_PX);
  const bottom = Math.max(top, Number(viewport.scrollTop || 0) + Number(viewport.clientHeight || 0) + VIRTUAL_OVERSCAN_PX);
  let start = 0;
  while (start < count - 1 && offsets[start + 1] < top) start += 1;
  let end = start;
  while (end < count - 1 && offsets[end] < bottom) end += 1;
  if (Number.isFinite(activeIndex) && activeIndex >= 0) {
    start = Math.min(start, Math.max(0, activeIndex - 2));
    end = Math.max(end, Math.min(count - 1, activeIndex + 2));
  }
  return {
    virtualized: true,
    start,
    end,
    topPadding: offsets[start],
    bottomPadding: Math.max(0, offsets[count] - offsets[end + 1]),
    items: entries.slice(start, end + 1).map((entry, offset) => {
      const index = start + offset;
      return { entry, index, key: keys[index] || chatRenderEntryKey(entry, index) };
    })
  };
}
