// The upcoming releases, read from the roadmap page of the documentation at build time.
import page from '../content/docs/docs/project/roadmap.md?raw';

export interface Release {
  version: string;
  milestone: string;
  brings: string;
}

const strip = (cell: string) => cell.replace(/`([^`]*)`/g, '$1').trim();

export const releases: Release[] = page
  .split('\n')
  .map((line) => line.match(/^\| \[(v[^\]]+)\]\(([^)]+)\) \| (.+) \|$/))
  .filter((m): m is RegExpMatchArray => m !== null)
  .map(([, version, milestone, brings]) => ({ version, milestone, brings: strip(brings) }));
