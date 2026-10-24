// The main navigation, shared by the site pages and the documentation header so both read the same.
export interface NavLink {
  label: string;
  href: string;
  /** The current page is in this section when its path starts with this prefix. */
  section: string;
}

export const repo = 'https://github.com/lorica-labs/volant';

export const nav: NavLink[] = [
  { label: 'Docs', href: '/docs/', section: '/docs/' },
  { label: 'Performance', href: '/performance/', section: '/performance/' },
  { label: 'Compatibility', href: '/compatibility/', section: '/compatibility/' },
  { label: 'Community', href: '/community/', section: '/community/' },
  { label: 'Blog', href: '/blog/', section: '/blog/' },
];

/** The label of the section `path` belongs to, if any. */
export const currentSection = (path: string) => nav.findLast((link) => path.startsWith(link.section))?.label;
