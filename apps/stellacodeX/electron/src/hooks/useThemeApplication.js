import { useEffect, useMemo, useState } from 'react';
import { effectiveThemeMode, themeCssVariables } from '../lib/theme';

function initialSystemTheme() {
  if (typeof window === 'undefined' || !window.matchMedia) return 'dark';
  return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}

export function useThemeApplication(settings) {
  const [systemTheme, setSystemTheme] = useState(initialSystemTheme);

  useEffect(() => {
    document.documentElement.dataset.theme = settings?.themeMode || 'system';
  }, [settings?.themeMode]);

  useEffect(() => {
    if (!window.matchMedia) return undefined;
    const query = window.matchMedia('(prefers-color-scheme: light)');
    const apply = () => setSystemTheme(query.matches ? 'light' : 'dark');
    apply();
    query.addEventListener?.('change', apply);
    return () => query.removeEventListener?.('change', apply);
  }, []);

  const activeThemeMode = effectiveThemeMode(settings?.themeMode, systemTheme);
  const themeVariables = useMemo(
    () => themeCssVariables(settings?.themeColors, activeThemeMode),
    [activeThemeMode, settings?.themeColors]
  );

  useEffect(() => {
    const root = document.documentElement;
    Object.entries(themeVariables).forEach(([name, value]) => {
      root.style.setProperty(name, value);
    });
  }, [themeVariables]);

  return { activeThemeMode, themeVariables };
}
