import type { APIRoute } from 'astro';
import { posts } from '../../lib/blog';

// The feed of the blog, written by hand: a list of items needs no library.
const escape = (text: string) =>
  text.replace(/[<>&'"]/g, (c) => ({ '<': '&lt;', '>': '&gt;', '&': '&amp;', "'": '&apos;', '"': '&quot;' })[c]!);

export const GET: APIRoute = async ({ site }) => {
  const home = new URL('/blog/', site);
  const items = (await posts())
    .map((post) => {
      const link = new URL(`/blog/${post.id}/`, site);
      return `    <item>
      <title>${escape(post.data.title)}</title>
      <link>${link}</link>
      <guid>${link}</guid>
      <pubDate>${post.data.date.toUTCString()}</pubDate>
      <description>${escape(post.data.description)}</description>
    </item>`;
    })
    .join('\n');
  const body = `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Volant blog</title>
    <link>${home}</link>
    <description>Releases, measurements and design notes from the Volant project.</description>
${items}
  </channel>
</rss>
`;
  return new Response(body, { headers: { 'Content-Type': 'application/rss+xml; charset=utf-8' } });
};
