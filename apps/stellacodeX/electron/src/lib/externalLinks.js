export function isExternalUrl(value) {
  return /^(https?:|mailto:)/i.test(String(value || '').trim());
}

export function openExternalUrl(value) {
  const url = String(value || '').trim();
  if (!isExternalUrl(url)) return false;
  window.stellacode2?.openExternal?.(url).catch(() => {});
  return true;
}

export function handleExternalLinkClick(event, href) {
  if (!openExternalUrl(href)) return;
  event.preventDefault();
  event.stopPropagation();
}
