// The two measured workloads of the performance page in the documentation, where the configuration
// and the recipes that reproduce them are described. Update both together. The name and detail of
// each workload are in the site strings (`performance.workloads` in src/i18n/ui/en.ts), by id.
export const measuredOn = '2026-09-27';
export const reference = 'ansible-core 2.19.12';

export interface Workload {
  id: 'roles' | 'k3s';
  /** Median of three timed passes, in seconds. */
  volant: number;
  ansible: number;
  recipe: string;
}

export const workloads: Workload[] = [
  {
    id: 'roles',
    volant: 0.529,
    ansible: 26.435,
    recipe: 'just bench',
  },
  {
    id: 'k3s',
    volant: 23.477,
    ansible: 50.519,
    recipe: 'just bench-k3s',
  },
];

export const ratio = (w: Workload) => w.ansible / w.volant;
