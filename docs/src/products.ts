// The products the documentation covers. The sidebar switcher lists them in this order; a product
// with its own docs gets an entry here and a sidebar group of its own.
export interface Product {
  name: string;
  description: string;
  /** The description in the other languages, by locale code. */
  translated?: Record<string, string>;
  href: string;
  colour: string;
  /** Path data for a 24 by 24 stroked icon drawn on the product's colour. */
  icon: string;
}

export const products: Product[] = [
  {
    name: 'Engine',
    description: 'Run Ansible playbooks with Volant',
    translated: {
      fr: 'Exécuter des playbooks Ansible avec Volant',
      de: 'Ansible-Playbooks mit Volant ausführen',
      es: 'Ejecutar playbooks de Ansible con Volant',
      'zh-cn': '用 Volant 运行 Ansible playbook',
    },
    href: '/docs/',
    colour: '#19b6a6',
    icon: 'M13 3L5 13.5h6L10 21l8-10.5h-6z',
  },
];
