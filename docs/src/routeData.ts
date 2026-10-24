import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { stripLocale } from './i18n';

// A page not translated yet shows the English text under the translation's prefix. Its canonical
// address is the English page, and it lists no other languages, so search engines index the text
// once, in English.
export const onRequest = defineRouteMiddleware((context) => {
  const route = context.locals.starlightRoute;
  if (!route.isFallback) return;
  const english = new URL(stripLocale(context.url.pathname), context.site).href;
  route.head = route.head.filter((entry) => !(entry.tag === 'link' && entry.attrs?.rel === 'alternate' && entry.attrs.hreflang));
  for (const entry of route.head) {
    if (entry.tag === 'link' && entry.attrs?.rel === 'canonical') entry.attrs.href = english;
    if (entry.tag === 'meta' && entry.attrs?.property === 'og:url') entry.attrs.content = english;
  }
});
