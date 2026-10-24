// The languages of the site and the documentation. English is the source: it lives at the root of
// the site, and every other language under its own prefix (/fr/, /de/ and so on).
export interface Locale {
  /** The path prefix, and the key of the documentation's locale in Starlight. */
  code: string;
  /** The BCP 47 tag, for the `lang` attribute. */
  lang: string;
  /** The name of the language in that language, as the language menus show it. */
  label: string;
}

export const locales: Locale[] = [
  { code: 'en', lang: 'en', label: 'English' },
  { code: 'fr', lang: 'fr', label: 'Français' },
  { code: 'de', lang: 'de', label: 'Deutsch' },
  { code: 'es', lang: 'es', label: 'Español' },
  { code: 'zh-cn', lang: 'zh-CN', label: '简体中文' },
];

export const defaultLocale = 'en';
export const codes = locales.map((l) => l.code);
export const localeOf = (code: string | undefined) => locales.find((l) => l.code === code) ?? locales[0];

/** The locale a path belongs to, read from its first segment. */
export function localeFromPath(path: string): string {
  const first = path.split('/')[1];
  return codes.includes(first) && first !== defaultLocale ? first : defaultLocale;
}

/** The path without its locale prefix: `/fr/docs/` gives `/docs/`. */
export function stripLocale(path: string): string {
  const code = localeFromPath(path);
  return code === defaultLocale ? path : path.slice(code.length + 1) || '/';
}

// Pages that exist in English only: the blog, the changelog and the files served as they are.
const englishOnly = ['/blog', '/docs/changelog', '/install.sh', '/favicon.svg', '/llms'];

/** `href` as seen from `code`: internal paths get the locale prefix, everything else is kept. */
export function localize(code: string, href: string): string {
  if (code === defaultLocale || !href.startsWith('/') || href.startsWith('//')) return href;
  if (englishOnly.some((p) => href.startsWith(p))) return href;
  // A heading is renamed in a translation, so its anchor is looked up; one not found keeps the
  // English anchor, which still lands on the page.
  const [path, anchor] = href.split('#');
  const translated = anchor && typeof __VOLANT_ANCHORS__ !== 'undefined' ? __VOLANT_ANCHORS__[`${code}:${path}#${anchor}`] : undefined;
  return `/${code}${path}${anchor ? `#${translated ?? anchor}` : ''}`;
}

/** The `getStaticPaths` of a site page: one path per language, English without a prefix. */
export const localePaths = () =>
  locales.map((l) => ({ params: { lang: l.code === defaultLocale ? undefined : l.code } }));
