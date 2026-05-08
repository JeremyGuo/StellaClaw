export const DEFAULT_THEME_COLORS = {
  light: {
    accent: '#339CFF',
    background: '#FFFFFF',
    foreground: '#1A1C1F',
    contrast: 45
  },
  dark: {
    accent: '#339CFF',
    background: '#181818',
    foreground: '#FFFFFF',
    contrast: 60
  }
};

const HEX_COLOR_RE = /^#[0-9a-fA-F]{6}$/;

export function normalizeHexColor(value, fallback) {
  const text = String(value || '').trim();
  if (!HEX_COLOR_RE.test(text)) return fallback;
  return text.toUpperCase();
}

export function normalizeContrast(value, fallback = 55) {
  const number = Number(value);
  if (!Number.isFinite(number)) return fallback;
  return Math.min(100, Math.max(0, Math.round(number)));
}

export function normalizeThemeColors(value) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    light: normalizeThemeVariant(source.light, DEFAULT_THEME_COLORS.light),
    dark: normalizeThemeVariant(source.dark, DEFAULT_THEME_COLORS.dark)
  };
}

function normalizeThemeVariant(value, fallback) {
  const source = value && typeof value === 'object' ? value : {};
  return {
    accent: normalizeHexColor(source.accent, fallback.accent),
    background: normalizeHexColor(source.background, fallback.background),
    foreground: normalizeHexColor(source.foreground, fallback.foreground),
    contrast: normalizeContrast(source.contrast, fallback.contrast)
  };
}

export function effectiveThemeMode(themeMode, systemTheme = 'dark') {
  if (themeMode === 'light' || themeMode === 'dark') return themeMode;
  return systemTheme === 'light' ? 'light' : 'dark';
}

export function themeCssVariables(themeColors, mode) {
  const normalized = normalizeThemeColors(themeColors);
  const colors = normalized[mode === 'light' ? 'light' : 'dark'];
  return mode === 'light' ? lightVariables(colors) : darkVariables(colors);
}

function lightVariables(colors) {
  const { accent, background, foreground, contrast } = colors;
  const borderAlpha = clamp(contrast / 420, 0.06, 0.22);
  return {
    '--bg': background,
    '--chrome': mix(background, foreground, 0.02),
    '--sidebar': mix(background, foreground, 0.04),
    '--sidebar-soft': mix(background, foreground, 0.07),
    '--panel': mix(background, foreground, 0.01),
    '--panel-2': background,
    '--panel-3': mix(background, foreground, 0.06),
    '--hover': mix(background, foreground, 0.10),
    '--border': rgba(foreground, borderAlpha),
    '--border-strong': rgba(foreground, borderAlpha + 0.07),
    '--text': foreground,
    '--muted': mix(foreground, background, 0.44),
    '--faint': mix(foreground, background, 0.62),
    '--accent': accent,
    '--accent-soft': rgba(accent, 0.14),
    '--link': accent,
    '--link-hover': mix(accent, foreground, 0.22),
    '--danger': '#C24132',
    '--green': accent,
    '--orange': '#B45F20',
    '--code-bg': mix(background, foreground, 0.04),
    '--code-inline-bg': rgba(foreground, 0.055),
    '--message-user-bg': mix(background, foreground, 0.035),
    '--message-user-border': rgba(foreground, 0.06),
    '--tool-row-bg': mix(background, foreground, 0.035),
    '--tool-row-hover': mix(background, foreground, 0.07),
    '--tool-chip-bg': mix(background, foreground, 0.08),
    '--tool-section-bg': rgba(foreground, 0.018),
    '--shadow': `0 24px 64px ${rgba(foreground, 0.10)}`
  };
}

function darkVariables(colors) {
  const { accent, background, foreground, contrast } = colors;
  const borderAlpha = clamp(contrast / 360, 0.08, 0.28);
  return {
    '--bg': background,
    '--chrome': mix(background, foreground, 0.04),
    '--sidebar': mix(background, foreground, 0.05),
    '--sidebar-soft': mix(background, foreground, 0.09),
    '--panel': background,
    '--panel-2': mix(background, foreground, 0.07),
    '--panel-3': mix(background, foreground, 0.12),
    '--hover': mix(background, foreground, 0.17),
    '--border': rgba(foreground, borderAlpha),
    '--border-strong': rgba(foreground, borderAlpha + 0.07),
    '--text': foreground,
    '--muted': mix(foreground, background, 0.42),
    '--faint': mix(foreground, background, 0.60),
    '--accent': accent,
    '--accent-soft': rgba(accent, 0.18),
    '--link': mix(accent, foreground, 0.14),
    '--link-hover': mix(accent, foreground, 0.34),
    '--danger': '#FF8A7A',
    '--green': accent,
    '--orange': '#EE9B57',
    '--code-bg': mix(background, '#000000', 0.22),
    '--code-inline-bg': rgba(foreground, 0.075),
    '--message-user-bg': mix(background, foreground, 0.08),
    '--message-user-border': rgba(foreground, 0.08),
    '--tool-row-bg': mix(background, foreground, 0.055),
    '--tool-row-hover': mix(background, foreground, 0.09),
    '--tool-chip-bg': mix(background, foreground, 0.10),
    '--tool-section-bg': rgba(foreground, 0.025),
    '--shadow': '0 26px 70px rgba(0, 0, 0, 0.38)'
  };
}

function hexToRgb(hex) {
  const text = normalizeHexColor(hex, '#000000').slice(1);
  return {
    r: parseInt(text.slice(0, 2), 16),
    g: parseInt(text.slice(2, 4), 16),
    b: parseInt(text.slice(4, 6), 16)
  };
}

function componentToHex(value) {
  return Math.round(value).toString(16).padStart(2, '0').toUpperCase();
}

function rgbToHex({ r, g, b }) {
  return `#${componentToHex(r)}${componentToHex(g)}${componentToHex(b)}`;
}

function mix(a, b, amount) {
  const left = hexToRgb(a);
  const right = hexToRgb(b);
  const ratio = clamp(amount, 0, 1);
  return rgbToHex({
    r: left.r + (right.r - left.r) * ratio,
    g: left.g + (right.g - left.g) * ratio,
    b: left.b + (right.b - left.b) * ratio
  });
}

function rgba(hex, alpha) {
  const { r, g, b } = hexToRgb(hex);
  return `rgba(${r}, ${g}, ${b}, ${clamp(alpha, 0, 1).toFixed(3)})`;
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}
