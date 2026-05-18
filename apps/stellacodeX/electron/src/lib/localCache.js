const LOCAL_CACHE_MAX_BYTES = 1_500_000;
const LOCAL_CACHE_PREFIX = 'stellacode.cache.v1';

export function localCacheKey(kind, parts) {
  return `${LOCAL_CACHE_PREFIX}:${kind}:${parts.map((part) => encodeURIComponent(String(part ?? ''))).join(':')}`;
}

export function readLocalCache(kind, parts) {
  if (typeof window === 'undefined' || !window.localStorage) return null;
  try {
    const raw = window.localStorage.getItem(localCacheKey(kind, parts));
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    return parsed?.value ?? null;
  } catch {
    return null;
  }
}

export function writeLocalCache(kind, parts, value) {
  if (typeof window === 'undefined' || !window.localStorage) return;
  try {
    const raw = JSON.stringify({ saved_at: Date.now(), value });
    if (raw.length > LOCAL_CACHE_MAX_BYTES) return;
    window.localStorage.setItem(localCacheKey(kind, parts), raw);
  } catch {
    // Cache writes are opportunistic.
  }
}

export function removeLocalCache(kind, parts) {
  if (typeof window === 'undefined' || !window.localStorage) return;
  try {
    window.localStorage.removeItem(localCacheKey(kind, parts));
  } catch {
    // Cache invalidation is best-effort.
  }
}
