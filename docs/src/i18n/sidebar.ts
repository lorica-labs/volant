// The sidebar labels of the documentation in every language but English, keyed by the English
// label. A label missing here shows in English.
import { localeOf } from './locales';

type Labels = Record<string, { fr: string; de: string; es: string; 'zh-cn': string }>;

export const sidebarLabels: Labels = {
  'Start here': { fr: 'Pour commencer', de: 'Erste Schritte', es: 'Para empezar', 'zh-cn': '入门' },
  'Why Volant': { fr: 'Pourquoi Volant', de: 'Warum Volant', es: 'Por qué Volant', 'zh-cn': '为什么选择 Volant' },
  Installation: { fr: 'Installation', de: 'Installation', es: 'Instalación', 'zh-cn': '安装' },
  Quickstart: { fr: 'Démarrage rapide', de: 'Schnellstart', es: 'Inicio rápido', 'zh-cn': '快速开始' },
  'Will my playbook run?': {
    fr: 'Mon playbook tournera-t-il ?',
    de: 'Läuft mein Playbook?',
    es: '¿Funcionará mi playbook?',
    'zh-cn': '我的 playbook 能运行吗？',
  },
  'Ansible command names': {
    fr: 'Les noms des commandes ansible',
    de: 'Ansible-Befehlsnamen',
    es: 'Nombres de comandos de ansible',
    'zh-cn': 'ansible 命令名',
  },
  'When a run fails': {
    fr: 'Quand une exécution échoue',
    de: 'Wenn ein Lauf fehlschlägt',
    es: 'Cuando una ejecución falla',
    'zh-cn': '运行失败时',
  },
  Playbooks: { fr: 'Playbooks', de: 'Playbooks', es: 'Playbooks', 'zh-cn': 'Playbook' },
  'Running plays': { fr: 'Exécuter des plays', de: 'Plays ausführen', es: 'Ejecutar plays', 'zh-cn': '运行 play' },
  'Roles, includes and imports': {
    fr: 'Rôles, includes et imports',
    de: 'Rollen, Includes und Imports',
    es: 'Roles, includes e imports',
    'zh-cn': '角色、include 与 import',
  },
  'Blocks and handlers': {
    fr: 'Blocs et handlers',
    de: 'Blöcke und Handler',
    es: 'Bloques y handlers',
    'zh-cn': '块与 handler',
  },
  'Loops and retries': {
    fr: 'Boucles et nouvelles tentatives',
    de: 'Schleifen und Wiederholungen',
    es: 'Bucles y reintentos',
    'zh-cn': '循环与重试',
  },
  'Delegation, meta and environment': {
    fr: 'Délégation, meta et environnement',
    de: 'Delegation, meta und Umgebung',
    es: 'Delegación, meta y entorno',
    'zh-cn': '委派、meta 与环境变量',
  },
  'Check mode, diff and listings': {
    fr: 'Mode check, diff et listes',
    de: 'Check-Modus, Diff und Listen',
    es: 'Modo check, diff y listados',
    'zh-cn': 'check 模式、diff 与列表',
  },
  Variables: { fr: 'Variables', de: 'Variablen', es: 'Variables', 'zh-cn': '变量' },
  'Precedence and facts': {
    fr: 'Priorité et facts',
    de: 'Vorrang und Facts',
    es: 'Precedencia y facts',
    'zh-cn': '优先级与 facts',
  },
  Templating: { fr: 'Templates', de: 'Templates', es: 'Plantillas', 'zh-cn': '模板' },
  Vault: { fr: 'Vault', de: 'Vault', es: 'Vault', 'zh-cn': 'Vault' },
  Hosts: { fr: 'Hôtes', de: 'Hosts', es: 'Hosts', 'zh-cn': '主机' },
  'Inventories and the fact cache': {
    fr: 'Inventaires et cache de facts',
    de: 'Inventare und Fact-Cache',
    es: 'Inventarios y caché de facts',
    'zh-cn': '清单与 fact 缓存',
  },
  'Connections and become': {
    fr: 'Connexions et become',
    de: 'Verbindungen und become',
    es: 'Conexiones y become',
    'zh-cn': '连接与 become',
  },
  'Cross-host batching': {
    fr: 'Regroupement entre hôtes',
    de: 'Hostübergreifendes Batching',
    es: 'Agrupación entre hosts',
    'zh-cn': '跨主机批处理',
  },
  Reference: { fr: 'Référence', de: 'Referenz', es: 'Referencia', 'zh-cn': '参考' },
  'Command line': { fr: 'Ligne de commande', de: 'Befehlszeile', es: 'Línea de comandos', 'zh-cn': '命令行' },
  Configuration: { fr: 'Configuration', de: 'Konfiguration', es: 'Configuración', 'zh-cn': '配置' },
  'Exit codes': { fr: 'Codes de sortie', de: 'Exit-Codes', es: 'Códigos de salida', 'zh-cn': '退出码' },
  Keywords: { fr: 'Mots-clés', de: 'Schlüsselwörter', es: 'Palabras clave', 'zh-cn': '关键字' },
  Modules: { fr: 'Modules', de: 'Module', es: 'Módulos', 'zh-cn': '模块' },
  'Full support matrix': {
    fr: 'Matrice de support complète',
    de: 'Vollständige Unterstützungsmatrix',
    es: 'Matriz de compatibilidad completa',
    'zh-cn': '完整支持矩阵',
  },
  'Template language': {
    fr: 'Langage de templates',
    de: 'Template-Sprache',
    es: 'Lenguaje de plantillas',
    'zh-cn': '模板语言',
  },
  'Managed runtime': {
    fr: 'Runtime géré',
    de: 'Verwaltete Laufzeit',
    es: 'Runtime gestionado',
    'zh-cn': '托管运行时',
  },
  'Switch without risk': {
    fr: 'Changer sans risque',
    de: 'Ohne Risiko umsteigen',
    es: 'Cambiar sin riesgo',
    'zh-cn': '无风险切换',
  },
  'Run recordings': {
    fr: 'Enregistrements de runs',
    de: 'Aufzeichnungen von Läufen',
    es: 'Grabaciones de ejecuciones',
    'zh-cn': '运行记录',
  },
  Glossary: { fr: 'Glossaire', de: 'Glossar', es: 'Glosario', 'zh-cn': '术语表' },
  'Under the hood': { fr: 'Sous le capot', de: 'Unter der Haube', es: 'Bajo el capó', 'zh-cn': '内部机制' },
  Architecture: { fr: 'Architecture', de: 'Architektur', es: 'Arquitectura', 'zh-cn': '架构' },
  'The agent on your hosts': {
    fr: "L'agent sur vos hôtes",
    de: 'Der Agent auf Ihren Hosts',
    es: 'El agente en tus hosts',
    'zh-cn': '主机上的 agent',
  },
  'The pre-flight check': {
    fr: 'La vérification préalable',
    de: 'Die Vorabprüfung',
    es: 'La comprobación previa',
    'zh-cn': '预检',
  },
  'Action plugins': { fr: "Plugins d'action", de: 'Action-Plugins', es: 'Plugins de acción', 'zh-cn': 'action 插件' },
  'The warm Python path': {
    fr: 'Le chemin Python à chaud',
    de: 'Der warme Python-Pfad',
    es: 'La ruta de Python en caliente',
    'zh-cn': '常驻 Python 路径',
  },
  'Decision records': {
    fr: 'Fiches de décision',
    de: 'Entscheidungsprotokolle',
    es: 'Registros de decisión',
    'zh-cn': '决策记录',
  },
  Project: { fr: 'Projet', de: 'Projekt', es: 'Proyecto', 'zh-cn': '项目' },
  Roadmap: { fr: 'Feuille de route', de: 'Roadmap', es: 'Hoja de ruta', 'zh-cn': '路线图' },
  Performance: { fr: 'Performances', de: 'Leistung', es: 'Rendimiento', 'zh-cn': '性能' },
  Tests: { fr: 'Tests', de: 'Tests', es: 'Pruebas', 'zh-cn': '测试' },
  Contributing: { fr: 'Contribuer', de: 'Mitwirken', es: 'Contribuir', 'zh-cn': '参与贡献' },
  Changelog: { fr: 'Journal des modifications', de: 'Änderungsprotokoll', es: 'Registro de cambios', 'zh-cn': '更新日志' },
  'Security policy': {
    fr: 'Politique de sécurité',
    de: 'Sicherheitsrichtlinie',
    es: 'Política de seguridad',
    'zh-cn': '安全策略',
  },
};

type Item = { label?: string; items?: Item[]; translations?: Record<string, string> } & Record<string, unknown>;

/** The sidebar with the translated label of every item and group that has one. */
export function translateSidebar<T extends Item>(items: T[]): T[] {
  return items.map((item) => {
    const labels = item.label ? sidebarLabels[item.label] : undefined;
    return {
      ...item,
      // Starlight looks the translations up by language tag (zh-CN), not by path prefix (zh-cn).
      ...(labels && {
        translations: Object.fromEntries(Object.entries(labels).map(([code, label]) => [localeOf(code).lang, label])),
      }),
      ...(item.items && { items: translateSidebar(item.items) }),
    };
  });
}
