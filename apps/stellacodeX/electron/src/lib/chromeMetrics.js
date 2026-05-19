function setPxVariable(element, name, value) {
  if (!Number.isFinite(value)) return;
  element.style.setProperty(name, `${value}px`);
}

export function applyChromeMetrics(metrics) {
  if (!metrics || typeof document === 'undefined') return;
  const root = document.documentElement;
  root.dataset.platform = metrics.platform || 'unknown';
  setPxVariable(root, '--window-controls-left-safe-area', metrics.leftSafeArea);
  setPxVariable(root, '--chrome-left-toolbar-offset', metrics.leftToolbarOffset);
  setPxVariable(root, '--chrome-title-left-offset', metrics.titleLeftOffset);
  setPxVariable(root, '--chrome-right-toolbar-offset', metrics.rightToolbarOffset);
  setPxVariable(root, '--chrome-title-right-offset', metrics.titleRightOffset);
  setPxVariable(root, '--chrome-title-right-offset-with-update', metrics.titleRightOffsetWithUpdate);
}
