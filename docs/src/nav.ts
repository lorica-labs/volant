// The main navigation, shared by the site pages and the documentation header so both read the same.
// Labels come from the strings of each language (`nav` in src/i18n/ui/en.ts).
import { localize, stripLocale } from './i18n/locales';
import type { Strings } from './i18n/ui/en';

export interface NavLink {
  key: keyof Strings['nav'];
  href: string;
  /** The current page is in this section when its path, without the language, starts with this. */
  section: string;
}

export const repo = 'https://github.com/lorica-labs/volant';

export const nav: NavLink[] = [
  { key: 'docs', href: '/docs/', section: '/docs/' },
  { key: 'performance', href: '/performance/', section: '/performance/' },
  { key: 'compatibility', href: '/compatibility/', section: '/compatibility/' },
  { key: 'community', href: '/community/', section: '/community/' },
  { key: 'blog', href: '/blog/', section: '/blog/' },
];

/** The key of the section `path` belongs to, if any. */
export const currentSection = (path: string) => nav.findLast((link) => stripLocale(path).startsWith(link.section))?.key;

/** The navigation as seen from a language: labels translated, links under its prefix. */
export const navFor = (code: string, t: Strings) =>
  nav.map((link) => ({ ...link, label: t.nav[link.key], href: localize(code, link.href) }));
