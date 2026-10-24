import { getCollection } from 'astro:content';

// The published posts, newest first. Drafts appear in `astro dev` so they can be read before they go
// out, and never in a build.
export const posts = async () =>
  (await getCollection('blog', (post) => import.meta.env.DEV || !post.data.draft)).sort(
    (a, b) => b.data.date.getTime() - a.data.date.getTime(),
  );

export const day = (date: Date) => date.toISOString().slice(0, 10);
