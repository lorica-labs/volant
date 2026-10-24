import { defineCollection } from 'astro:content';
import { z } from 'astro/zod';
import { glob } from 'astro/loaders';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';
import { changelogsLoader } from 'starlight-changelogs/loader';

export const collections = {
  // Posts of the project blog, one Markdown file each. A draft shows in `astro dev` only.
  blog: defineCollection({
    loader: glob({ pattern: '*.md', base: './src/content/blog' }),
    schema: z.object({
      title: z.string(),
      description: z.string(),
      date: z.coerce.date(),
      draft: z.boolean().default(false),
    }),
  }),
  docs: defineCollection({
    loader: docsLoader(),
    // `sourceHash` marks which English text a translated page was made from: see src/i18n/source-hash.mjs.
    schema: docsSchema({ extend: z.object({ sourceHash: z.string().optional() }) }),
  }),
  changelogs: defineCollection({
    loader: changelogsLoader([
      { provider: 'keep-a-changelog', base: 'docs/changelog', changelog: '../CHANGELOG.md', title: 'Changelog' },
    ]),
  }),
};
