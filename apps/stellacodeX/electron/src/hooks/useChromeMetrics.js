import { useEffect, useLayoutEffect } from 'react';
import { applyChromeMetrics } from '../lib/chromeMetrics';

export function useChromeMetrics() {
  useLayoutEffect(() => {
    applyChromeMetrics(window.stellacode2?.chromeMetrics?.());
  }, []);

  useEffect(() => {
    let frame = 0;
    let resolutionQuery = null;
    let removeResolutionListener = null;
    const refreshChromeMetrics = () => {
      if (frame) window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(() => {
        frame = 0;
        applyChromeMetrics(window.stellacode2?.chromeMetrics?.());
      });
    };
    const bindResolutionListener = () => {
      removeResolutionListener?.();
      if (!window.matchMedia) return;
      resolutionQuery = window.matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
      const listener = () => {
        refreshChromeMetrics();
        bindResolutionListener();
      };
      resolutionQuery.addEventListener?.('change', listener);
      removeResolutionListener = () => resolutionQuery?.removeEventListener?.('change', listener);
    };
    window.addEventListener('resize', refreshChromeMetrics);
    bindResolutionListener();
    return () => {
      if (frame) window.cancelAnimationFrame(frame);
      window.removeEventListener('resize', refreshChromeMetrics);
      removeResolutionListener?.();
    };
  }, []);
}
