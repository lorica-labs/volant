import { en, type Deep, type Strings } from './ui/en';
import { fr } from './ui/fr';
import { de } from './ui/de';
import { es } from './ui/es';
import { zhCn } from './ui/zh-cn';
import { defaultLocale, localize } from './locales';

export * from './locales';

const translations: Record<string, Deep<Strings>> = { fr, de, es, 'zh-cn': zhCn };

// A translation may leave keys out: they fall back to English, one level at a time.
function merge<T extends object>(base: T, over: Deep<T> | undefined): T {
  if (!over) return base;
  const out = { ...base } as Record<string, unknown>;
  for (const [key, value] of Object.entries(over)) {
    const current = (base as Record<string, unknown>)[key];
    out[key] =
      value && typeof value === 'object' && !Array.isArray(value) && current && typeof current === 'object'
        ? merge(current as object, value as Deep<object>)
        : (value ?? current);
  }
  return out as T;
}

/** The strings of `code`, English where the translation has nothing. */
export const strings = (code: string): Strings =>
  code === defaultLocale ? en : merge(en, translations[code]);

export type { Strings };

/** `text` with each `{name}` replaced by `vars.name`. */
export const fill = (text: string, vars: Record<string, string | number> = {}) =>
  text.replace(/\{(\w+)\}/g, (all, name) => (name in vars ? String(vars[name]) : all));

/** A string of inline HTML as seen from `code`: placeholders filled, internal links localized. */
export const html = (code: string, text: string, vars?: Record<string, string | number>) =>
  fill(text, vars).replace(/href="([^"]*)"/g, (_, href) => `href="${localize(code, href)}"`);
