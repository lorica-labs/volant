// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';
import starlightChangelogs from 'starlight-changelogs';
import starlightLinksValidator from 'starlight-links-validator';
import starlightLlmsTxt from 'starlight-llms-txt';
import starlightScrollToTop from 'starlight-scroll-to-top';
import { readFileSync } from 'node:fs';

const repo = 'https://github.com/lorica-labs/volant';
// The header shows the release the documentation describes, read from the workspace manifest.
const version = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8').match(/^version = "(.+)"$/m)[1];

export default defineConfig({
  site: 'https://volant.sh',
  // The overview used to live here; old links land on the home page.
  // Old addresses keep working after pages were merged or moved.
  redirects: {
    '/start/why': '/',
    '/playbooks/serial': '/playbooks/how-a-play-runs/',
    '/playbooks/includes': '/playbooks/roles/',
    '/playbooks/handlers': '/playbooks/blocks/',
    '/playbooks/meta': '/playbooks/delegation/',
    '/playbooks/environment': '/playbooks/delegation/',
    '/playbooks/tags': '/playbooks/check-and-diff/',
    '/playbooks/preflight': '/internals/preflight/',
    '/variables/facts': '/variables/precedence/',
    '/variables/python-helper': '/variables/templating/',
    '/variables/trust': '/variables/templating/',
    '/hosts/fact-cache': '/hosts/inventories/',
    '/hosts/become': '/hosts/connections/',
    '/reference/action-plugins': '/internals/action-plugins/',
  },
  vite: { define: { __VOLANT_VERSION__: JSON.stringify(version) } },
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
        starlightLinksValidator(),
        // Renders CHANGELOG.md as pages under /changelog/.
        starlightChangelogs(),
        // Publishes /llms.txt and the full text of the site for tools that read documentation.
        starlightLlmsTxt({ projectName: 'Volant' }),
        starlightScrollToTop(),
      ],
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'Why Volant', slug: '' },
            { label: 'Installation', slug: 'start/installation' },
            { label: 'Quickstart', slug: 'start/quickstart' },
            { label: 'Will my playbook run?', slug: 'start/compatibility' },
            { label: 'When a run fails', slug: 'start/troubleshooting' },
          ],
        },
        {
          label: 'Playbooks',
          items: [
            { label: 'Running plays', slug: 'playbooks/how-a-play-runs' },
            { label: 'Roles, includes and imports', slug: 'playbooks/roles' },
            { label: 'Blocks and handlers', slug: 'playbooks/blocks' },
            { label: 'Loops and retries', slug: 'playbooks/loops' },
            { label: 'Delegation, meta and environment', slug: 'playbooks/delegation' },
            { label: 'Check mode, diff and listings', slug: 'playbooks/check-and-diff' },
          ],
        },
        {
          label: 'Variables',
          items: [
            { label: 'Precedence and facts', slug: 'variables/precedence' },
            { label: 'Templating', slug: 'variables/templating' },
            { label: 'Vault', slug: 'variables/vault' },
          ],
        },
        {
          label: 'Hosts',
          items: [
            { label: 'Inventories and the fact cache', slug: 'hosts/inventories' },
            { label: 'Connections and become', slug: 'hosts/connections' },
            { label: 'Cross-host batching', slug: 'hosts/batching' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Command line', slug: 'reference/cli' },
            { label: 'Configuration', slug: 'reference/configuration' },
            { label: 'Exit codes', slug: 'reference/exit-codes' },
            { label: 'Keywords', slug: 'reference/keywords' },
            { label: 'Modules', slug: 'reference/modules' },
            { label: 'Full support matrix', slug: 'reference/parity' },
            { label: 'Template language', slug: 'reference/templates' },
            { label: 'Managed runtime', slug: 'reference/runtime' },
            { label: 'Glossary', slug: 'reference/glossary' },
          ],
        },
        {
          label: 'Under the hood',
          items: [
            { label: 'Architecture', slug: 'internals/architecture' },
            { label: 'The agent on your hosts', slug: 'internals/agent' },
            { label: 'The pre-flight check', slug: 'internals/preflight' },
            { label: 'Action plugins', slug: 'internals/action-plugins' },
            { label: 'The warm Python path', slug: 'internals/python' },
            { label: 'Decision records', items: [{ autogenerate: { directory: 'decisions' } }] },
          ],
        },
        {
          label: 'Project',
          items: [
            { label: 'Roadmap', slug: 'project/roadmap' },
            { label: 'Performance', slug: 'project/performance' },
            { label: 'Tests', slug: 'project/tests' },
            { label: 'Contributing', link: `${repo}/blob/main/CONTRIBUTING.md`, attrs: { target: '_blank' } },
            { label: 'Changelog', link: '/changelog/' },
            { label: 'Security policy', link: `${repo}/blob/main/SECURITY.md`, attrs: { target: '_blank' } },
          ],
        },
      ],
    }),
  ],
});
