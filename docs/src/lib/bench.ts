// The two measured workloads of the performance page in the documentation, where the configuration
// and the recipes that reproduce them are described. Update both together.
export const measuredOn = '2026-09-27';
export const reference = 'ansible-core 2.19.12 with pipelining = True';

export interface Workload {
  name: string;
  detail: string;
  /** Median of three timed passes, in seconds. */
  volant: number;
  ansible: number;
  recipe: string;
}

export const workloads: Workload[] = [
  {
    name: 'Four Galaxy roles',
    detail: 'geerlingguy.security, nginx, git and pip on two converged Ubuntu 24.04 hosts, 56 tasks per host',
    volant: 0.529,
    ansible: 26.435,
    recipe: 'just bench',
  },
  {
    name: 'k3s-ansible site.yml',
    detail: 'the official k3s-ansible playbook against an installed cluster, one server and one agent',
    volant: 23.477,
    ansible: 50.519,
    recipe: 'just bench-k3s',
  },
];

export const ratio = (w: Workload) => w.ansible / w.volant;
