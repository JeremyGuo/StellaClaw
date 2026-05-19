import { useEffect, useState } from 'react';

export function useUpdaterStatus() {
  const [updaterStatus, setUpdaterStatus] = useState({ state: 'idle' });

  useEffect(() => {
    const updater = window.stellacode2?.updater;
    if (!updater) return undefined;
    let disposed = false;
    const applyStatus = (status) => {
      if (!disposed && status) {
        setUpdaterStatus(status);
      }
    };
    updater.status?.().then(applyStatus).catch(() => {});
    const unsubscribe = updater.onStatus?.(applyStatus);
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, []);

  return updaterStatus;
}
