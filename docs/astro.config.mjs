// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';
import starlightChangelogs from 'starlight-changelogs';
import starlightLinksValidator from 'starlight-links-validator';
import starlightLlmsTxt from 'starlight-llms-txt';
import starlightScrollToTop from 'starlight-scroll-to-top';
import { readFileSync, readdirSync } from 'node:fs';
import { sep } from 'node:path';
import { locales } from './src/i18n/locales.ts';
import { translateSidebar } from './src/i18n/sidebar.ts';
import { anchorMap, staleTranslations } from './src/i18n/source-hash.mjs';

const repo = 'https://github.com/lorica-labs/volant';
// The header shows the release the documentation describes, read from the workspace manifest.
const version = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8').match(/^version = "(.+)"$/m)[1];

// The documentation used to live at the root of the site. Every page it had there redirects to the
// same page under /docs/, and the pages merged since then redirect to the page that holds them now.
const docsDir = new URL('./src/content/docs/docs/', import.meta.url);
const moved = Object.fromEntries(
  readdirSync(docsDir, { recursive: true })
    .map((file) => String(file).split(sep).join('/'))
    .filter((file) => /\.mdx?$/.test(file) && file !== 'index.mdx')
    .map((file) => file.replace(/\.mdx?$/, ''))
    .map((page) => [`/${page}`, `/docs/${page}/`]),
);
const merged = {
  '/start/why': '/docs/',
  '/playbooks/serial': '/docs/playbooks/how-a-play-runs/',
  '/playbooks/includes': '/docs/playbooks/roles/',
  '/playbooks/handlers': '/docs/playbooks/blocks/',
  '/playbooks/meta': '/docs/playbooks/delegation/',
  '/playbooks/environment': '/docs/playbooks/delegation/',
  '/playbooks/tags': '/docs/playbooks/check-and-diff/',
  '/playbooks/preflight': '/docs/internals/preflight/',
  '/variables/facts': '/docs/variables/precedence/',
  '/variables/python-helper': '/docs/variables/templating/',
  '/variables/trust': '/docs/variables/templating/',
  '/hosts/fact-cache': '/docs/hosts/inventories/',
  '/hosts/become': '/docs/hosts/connections/',
  '/reference/action-plugins': '/docs/internals/action-plugins/',
  '/changelog': '/docs/changelog/',
};
const redirects = { ...moved, ...merged };

// Translated pages whose English page changed since they were translated show a notice.
const stale = staleTranslations();
if (stale.length) console.warn(`[i18n] ${stale.length} translated pages are behind their English page`);

export default defineConfig({
  site: 'https://volant.sh',
  // The overview used to live here; old links land on the home page.
  // Old addresses keep working after pages were merged or moved.
  redirects,
  vite: {
    define: {
      __VOLANT_VERSION__: JSON.stringify(version),
      __VOLANT_STALE__: JSON.stringify(stale),
      __VOLANT_ANCHORS__: JSON.stringify(anchorMap()),
    },
  },
  integrations: [
    // Mermaid has to come before Starlight so its code fences are claimed first. The colors come
    // from the site's own tokens in custom.css, so one render serves both light and dark.
    mermaid({
      theme: 'base',
      autoTheme: false,
      mermaidConfig: {
        fontFamily: '"Geist Variable", ui-sans-serif, system-ui, sans-serif',
        themeVariables: { fontFamily: '"Geist Variable", ui-sans-serif, system-ui, sans-serif', fontSize: '14px' },
        flowchart: { curve: 'basis', padding: 14, nodeSpacing: 40, rankSpacing: 52 },
        sequence: { mirrorActors: false, actorMargin: 48, boxMargin: 8, noteMargin: 12, messageMargin: 32, width: 140 },
      },
    }),
    starlight({
      title: 'Volant',
      // English is the source and lives at the root; every translation under its own prefix. A page
      // not translated yet shows the English text with a notice.
      defaultLocale: 'root',
      locales: Object.fromEntries(
        locales.map((l) => [l.code === 'en' ? 'root' : l.code, { label: l.label, lang: l.lang }]),
      ),
      description: 'Run your Ansible playbooks with a fast engine written in Rust.',
      logo: { src: './assets/volant-mark.svg' },
      favicon: '/favicon.svg',
      social: [{ icon: 'github', label: 'GitHub', href: repo }],
      editLink: { baseUrl: `${repo}/edit/main/docs/` },
      lastUpdated: true,
      customCss: ['./src/styles/custom.css'],
      // Expressive Code computes with these colors at build time, so they are real values per
      // theme rather than the site's CSS variables.
      expressiveCode: {
        themes: ['vitesse-dark', 'vitesse-light'],
        styleOverrides: {
          borderRadius: '0.75rem',
          borderColor: ({ theme }) => (theme.type === 'dark' ? '#22282d' : '#dde3e6'),
          codeBackground: ({ theme }) => (theme.type === 'dark' ? '#111417' : '#ffffff'),
          codeFontFamily: "'Geist Mono Variable', ui-monospace, monospace",
          codeFontSize: '0.85rem',
          uiFontFamily: "'Geist Variable', system-ui, sans-serif",
          frames: {
            shadowColor: 'transparent',
            frameBoxShadowCssValue: 'none',
            editorActiveTabBackground: ({ theme }) => (theme.type === 'dark' ? '#111417' : '#ffffff'),
            editorActiveTabIndicatorTopColor: 'transparent',
            editorTabBarBackground: ({ theme }) => (theme.type === 'dark' ? '#171b1f' : '#f5f7f8'),
            editorTabBarBorderBottomColor: ({ theme }) => (theme.type === 'dark' ? '#22282d' : '#dde3e6'),
            terminalBackground: ({ theme }) => (theme.type === 'dark' ? '#111417' : '#ffffff'),
            terminalTitlebarBackground: ({ theme }) => (theme.type === 'dark' ? '#171b1f' : '#f5f7f8'),
            terminalTitlebarBorderBottomColor: ({ theme }) => (theme.type === 'dark' ? '#22282d' : '#dde3e6'),
          },
        },
      },
      components: {
        Head: './src/components/Head.astro',
        EditLink: './src/components/EditLink.astro',
        Header: './src/components/Header.astro',
        PageTitle: './src/components/PageTitle.astro',
        Sidebar: './src/components/Sidebar.astro',
      },
      plugins: [
        // Fails the build on a broken internal link or anchor.
        // Links to a page not translated yet are fine: Starlight shows the English page there.
        starlightLinksValidator({ errorOnFallbackPages: false }),
        // Renders CHANGELOG.md as pages under /changelog/.
        starlightChangelogs(),
        // Publishes /llms.txt and the full text of the site for tools that read documentation.
        starlightLlmsTxt({ projectName: 'Volant' }),
        starlightScrollToTop(),
      ],
      sidebar: translateSidebar([
        {
          label: 'Start here',
          items: [
            { label: 'Why Volant', slug: 'docs' },
            { label: 'Installation', slug: 'docs/start/installation' },
            { label: 'Quickstart', slug: 'docs/start/quickstart' },
            { label: 'Will my playbook run?', slug: 'docs/start/compatibility' },
            { label: 'When a run fails', slug: 'docs/start/troubleshooting' },
          ],
        },
        {
          label: 'Playbooks',
          items: [
            { label: 'Running plays', slug: 'docs/playbooks/how-a-play-runs' },
            { label: 'Roles, includes and imports', slug: 'docs/playbooks/roles' },
            { label: 'Blocks and handlers', slug: 'docs/playbooks/blocks' },
            { label: 'Loops and retries', slug: 'docs/playbooks/loops' },
            { label: 'Delegation, meta and environment', slug: 'docs/playbooks/delegation' },
            { label: 'Check mode, diff and listings', slug: 'docs/playbooks/check-and-diff' },
          ],
        },
        {
          label: 'Variables',
          items: [
            { label: 'Precedence and facts', slug: 'docs/variables/precedence' },
            { label: 'Templating', slug: 'docs/variables/templating' },
            { label: 'Vault', slug: 'docs/variables/vault' },
          ],
        },
        {
          label: 'Hosts',
          items: [
            { label: 'Inventories and the fact cache', slug: 'docs/hosts/inventories' },
            { label: 'Connections and become', slug: 'docs/hosts/connections' },
            { label: 'Cross-host batching', slug: 'docs/hosts/batching' },
          ],
        },
        {
          label: 'Reference',
          items: [
            {
              label: 'Command line',
              items: [
                { label: 'volant playbook', slug: 'docs/reference/cli' },
                { label: 'volant adhoc', slug: 'docs/reference/cli/adhoc' },
                { label: 'volant doc', slug: 'docs/reference/cli/doc' },
                { label: 'volant inventory', slug: 'docs/reference/cli/inventory' },
                { label: 'volant runtime', slug: 'docs/reference/cli/runtime' },
                { label: 'volant shims', slug: 'docs/reference/cli/shims' },
                { label: 'volant vault', slug: 'docs/reference/cli/vault' },
              ],
            },
            { label: 'Configuration', slug: 'docs/reference/configuration' },
            { label: 'Exit codes', slug: 'docs/reference/exit-codes' },
            { label: 'Keywords', slug: 'docs/reference/keywords' },
            { label: 'Modules', slug: 'docs/reference/modules' },
            { label: 'Full support matrix', slug: 'docs/reference/parity' },
            { label: 'Template language', slug: 'docs/reference/templates' },
            { label: 'Managed runtime', slug: 'docs/reference/runtime' },
            { label: 'Glossary', slug: 'docs/reference/glossary' },
          ],
        },
        {
          label: 'Under the hood',
          items: [
            { label: 'Architecture', slug: 'docs/internals/architecture' },
            { label: 'The agent on your hosts', slug: 'docs/internals/agent' },
            { label: 'The pre-flight check', slug: 'docs/internals/preflight' },
            { label: 'Action plugins', slug: 'docs/internals/action-plugins' },
            { label: 'The warm Python path', slug: 'docs/internals/python' },
            { label: 'Decision records', items: [{ autogenerate: { directory: 'docs/decisions' } }] },
          ],
        },
        {
          label: 'Project',
          items: [
            { label: 'Roadmap', slug: 'docs/project/roadmap' },
            { label: 'Performance', slug: 'docs/project/performance' },
            { label: 'Tests', slug: 'docs/project/tests' },
            { label: 'Contributing', link: `${repo}/blob/main/CONTRIBUTING.md`, attrs: { target: '_blank' } },
            { label: 'Changelog', link: '/docs/changelog/' },
            { label: 'Security policy', link: `${repo}/blob/main/SECURITY.md`, attrs: { target: '_blank' } },
          ],
        },
      ]),
    }),
  ],
});
