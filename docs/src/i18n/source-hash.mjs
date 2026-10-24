// A translated documentation page records which English text it was translated from, as a hash of
// the English page's body in its `sourceHash` field. When the English page changes, the hashes no
// longer match and the translated page shows that it may be out of date.
//
//   node docs/src/i18n/source-hash.mjs          stamp every translation with its source's hash
//   node docs/src/i18n/source-hash.mjs --check  list the translations whose source changed
//
// Stamp a page only after its translation has caught up with the English page.
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import GithubSlugger from 'github-slugger';

const content = fileURLToPath(new URL('../content/docs/', import.meta.url));
const translations = ['fr', 'de', 'es', 'zh-cn'];

/** The body of a page, without its frontmatter, as the hash sees it. */
export function body(text) {
  const normal = text.replace(/\r\n/g, '\n');
  const end = normal.startsWith('---\n') ? normal.indexOf('\n---\n', 4) : -1;
  return end === -1 ? normal : normal.slice(end + 5);
}

export const hashOf = (text) => createHash('sha256').update(body(text)).digest('hex').slice(0, 12);

/** A hash made only of digits, or digits around one `e`, would be read as a number in YAML. */
const quoted = (hash) => (/^\d+(e\d+)?$/.test(hash) ? `"${hash}"` : hash);

/** The English page a translated page comes from: `fr/docs/x.md` comes from `docs/x.md`. */
export function sourceOf(file) {
  const rel = relative(content, file).split(sep);
  return translations.includes(rel[0]) ? join(content, ...rel.slice(1)) : null;
}

function* pages(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* pages(path);
    else if (/\.mdx?$/.test(entry.name)) yield path;
  }
}

/** The ids of the translated pages whose English source changed since they were stamped. */
export function staleTranslations() {
  const stale = [];
  for (const code of translations) {
    const root = join(content, code);
    if (!existsSync(root)) continue;
    for (const file of pages(root)) {
      const source = sourceOf(file);
      if (!source || !existsSync(source)) continue;
      const have = readFileSync(file, 'utf8').match(/^sourceHash: "?(\w+)"?$/m)?.[1];
      if (have !== hashOf(readFileSync(source, 'utf8'))) {
        stale.push(relative(content, file).split(sep).join('/').replace(/\.mdx?$/, '').replace(/\/index$/, ''));
      }
    }
  }
  return stale;
}

/** The headings of a page, outside code blocks, as their ids, in order. */
function headingIds(text) {
  const slugger = new GithubSlugger();
  const ids = [];
  let fenced = false;
  for (const line of body(text).split('\n')) {
    if (/^\s*(```|~~~)/.test(line)) fenced = !fenced;
    const m = !fenced && line.match(/^#{2,4}\s+(.+?)\s*#*\s*$/);
    if (m) ids.push(slugger.slug(m[1].replace(/`/g, '').replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')));
  }
  return ids;
}

/**
 * Where a heading of an English page is in each translation: `{ 'fr:/docs/x/#uninstall':
 * 'désinstaller' }`. A translation keeps the headings of its source in the same order, so the n-th
 * heading of one is the n-th of the other. Pages whose heading counts differ are left out.
 */
export function anchorMap() {
  const map = {};
  for (const code of translations) {
    const root = join(content, code);
    if (!existsSync(root)) continue;
    for (const file of pages(root)) {
      const source = sourceOf(file);
      if (!source || !existsSync(source)) continue;
      const from = headingIds(readFileSync(source, 'utf8'));
      const to = headingIds(readFileSync(file, 'utf8'));
      if (from.length !== to.length) continue;
      const page = `/${relative(content, source).split(sep).join('/').replace(/\.mdx?$/, '').replace(/\/?index$/, '')}/`;
      from.forEach((id, i) => (map[`${code}:${page}#${id}`] = to[i]));
    }
  }
  return map;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const check = process.argv.includes('--check');
  let stale = 0;
  for (const code of translations) {
    const root = join(content, code);
    if (!existsSync(root)) continue;
    for (const file of pages(root)) {
      const source = sourceOf(file);
      if (!source || !existsSync(source)) continue;
      const want = hashOf(readFileSync(source, 'utf8'));
      const text = readFileSync(file, 'utf8');
      const have = text.match(/^sourceHash: "?(\w+)"?$/m)?.[1];
      if (have === want) continue;
      if (check) {
        stale++;
        console.log(`${relative(content, file)}: ${have ? 'source changed' : 'never stamped'}`);
      } else {
        const stamped = have
          ? text.replace(/^sourceHash: \S+$/m, `sourceHash: ${quoted(want)}`)
          : text.replace(/^---\r?\n/, `---\nsourceHash: ${want}\n`);
        writeFileSync(file, stamped);
      }
    }
  }
  if (check && stale) process.exitCode = 1;
}
