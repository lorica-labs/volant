// The summary of the support matrix, read from the generated page at build time, so the site never
// states a number the engine's own tests did not produce.
import page from '../content/docs/docs/reference/parity.md?raw';

export interface Category {
  name: string;
  runs: number;
  partial: number;
  notYet: number;
  later: number;
}

const start = page.indexOf('## Summary');
const table = page.slice(start, page.indexOf('\n## ', start + 1));

export const categories: Category[] = table
  .split('\n')
  .filter((line) => /^\| [A-Za-z]/.test(line) && !line.startsWith('| Category'))
  .map((line) => {
    const [name, runs, partial, notYet, later] = line.split('|').slice(1, 6).map((cell) => cell.trim());
    return { name, runs: +runs, partial: +partial, notYet: +notYet, later: +later };
  });

export const category = (name: string) => {
  const found = categories.find((c) => c.name === name);
  if (!found) throw new Error(`no category '${name}' in the support matrix summary`);
  return found;
};

/** ansible-core's own names, without the collections. */
export const core = categories.filter((c) => !c.name.startsWith('collection'));
/** The names that come from collections in the ansible package. */
export const collections = categories.filter((c) => c.name.startsWith('collection'));

const sum = (list: Category[], key: keyof Omit<Category, 'name'>) => list.reduce((n, c) => n + c[key], 0);
export const total = (list: Category[]) => sum(list, 'runs') + sum(list, 'partial') + sum(list, 'notYet') + sum(list, 'later');
export const running = (list: Category[]) => sum(list, 'runs');
export const partly = (list: Category[]) => sum(list, 'partial');
export const later = (list: Category[]) => sum(list, 'later');
