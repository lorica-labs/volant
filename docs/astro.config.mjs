// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';
import starlightChangelogs from 'starlight-changelogs';
import starlightLinksValidator from 'starlight-links-validator';
import starlightLlmsTxt from 'starlight-llms-txt';
import starlightScrollToTop from 'starlight-scroll-to-top';

const repo = 'https://github.com/lorica-labs/volant';

export default defineConfig({
  site: 'https://volant.sh',
  // The overview used to live here; old links land on the home page.
  redirects: { '/start/why': '/' },
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
          borderColor: ({ theme }) => (theme.type === 'dark' ? '#2a3438' : '#dde3e6'),
          codeBackground: ({ theme }) => (theme.type === 'dark' ? '#151d21' : '#ffffff'),
          codeFontFamily: "'Geist Mono Variable', ui-monospace, monospace",
          codeFontSize: '0.85rem',
          uiFontFamily: "'Geist Variable', system-ui, sans-serif",
          frames: {
            shadowColor: 'transparent',
            frameBoxShadowCssValue: 'none',
            editorActiveTabBackground: ({ theme }) => (theme.type === 'dark' ? '#151d21' : '#ffffff'),
            editorActiveTabIndicatorTopColor: 'transparent',
            editorTabBarBackground: ({ theme }) => (theme.type === 'dark' ? '#182125' : '#f5f7f8'),
            editorTabBarBorderBottomColor: ({ theme }) => (theme.type === 'dark' ? '#2a3438' : '#dde3e6'),
            terminalBackground: ({ theme }) => (theme.type === 'dark' ? '#151d21' : '#ffffff'),
            terminalTitlebarBackground: ({ theme }) => (theme.type === 'dark' ? '#182125' : '#f5f7f8'),
            terminalTitlebarBorderBottomColor: ({ theme }) => (theme.type === 'dark' ? '#2a3438' : '#dde3e6'),
          },
        },
      },
      components: {
        Head: './src/components/Head.astro',
        SocialIcons: './src/components/HeaderLinks.astro',
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
          ],
        },
        {
          label: 'Playbooks',
          collapsed: true,
          items: [
            { label: 'How a play runs', slug: 'playbooks/how-a-play-runs' },
            { label: 'Roles', slug: 'playbooks/roles' },
            { label: 'Blocks, rescue and always', slug: 'playbooks/blocks' },
            { label: 'Handlers', slug: 'playbooks/handlers' },
            { label: 'Tags and listings', slug: 'playbooks/tags' },
            { label: 'Loops and retries', slug: 'playbooks/loops' },
            { label: 'Serial batches', slug: 'playbooks/serial' },
            { label: 'Includes and imports', slug: 'playbooks/includes' },
            { label: 'Delegation and run_once', slug: 'playbooks/delegation' },
            { label: 'Environment and no_log', slug: 'playbooks/environment' },
            { label: 'What is not supported yet', slug: 'playbooks/preflight' },
          ],
        },
        {
          label: 'Variables',
          collapsed: true,
          items: [
            { label: 'Precedence', slug: 'variables/precedence' },
            { label: 'Templating', slug: 'variables/templating' },
            { label: 'Trusted and untrusted values', slug: 'variables/trust' },
            { label: 'Facts', slug: 'variables/facts' },
          ],
        },
        {
          label: 'Hosts and connections',
          collapsed: true,
          items: [
            { label: 'Inventories and patterns', slug: 'hosts/inventories' },
            { label: 'Connections and the agent', slug: 'hosts/connections' },
            { label: 'Privilege escalation', slug: 'hosts/become' },
            { label: 'Cross-host batching', slug: 'hosts/batching' },
          ],
        },
        {
          label: 'Reference',
          collapsed: true,
          items: [
            { label: 'Command line', slug: 'reference/cli' },
            { label: 'Configuration', slug: 'reference/configuration' },
            { label: 'Keywords', slug: 'reference/keywords' },
            { label: 'Modules', slug: 'reference/modules' },
            { label: 'Action plugins', slug: 'reference/action-plugins' },
            { label: 'Exit codes', slug: 'reference/exit-codes' },
            { label: 'Glossary', slug: 'reference/glossary' },
          ],
        },
        {
          label: 'Under the hood',
          collapsed: true,
          items: [
            { label: 'Architecture', slug: 'internals/architecture' },
            { label: 'The warm Python path', slug: 'internals/python' },
            { label: 'Decision records', items: [{ autogenerate: { directory: 'decisions' } }] },
          ],
        },
        {
          label: 'Project',
          collapsed: true,
          items: [
            { label: 'Roadmap', slug: 'project/roadmap' },
            { label: 'Contributing', link: `${repo}/blob/main/CONTRIBUTING.md`, attrs: { target: '_blank' } },
            { label: 'Changelog', link: '/changelog/' },
            { label: 'Security policy', link: `${repo}/blob/main/SECURITY.md`, attrs: { target: '_blank' } },
          ],
        },
      ],
    }),
  ],
});
