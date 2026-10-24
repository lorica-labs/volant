import type { Deep, Strings } from './en';

// Translation of en.ts. A key left out shows in English.
export const de: Deep<Strings> = {
  nav: {
    docs: 'Doku',
    performance: 'Performance',
    compatibility: 'Kompatibilität',
    community: 'Community',
    blog: 'Blog',
  },
  header: {
    home: 'Volant, Startseite',
    install: 'Installieren',
    menu: 'Menü',
    releaseNotes: 'Release Notes',
    github: 'Volant auf GitHub',
    language: 'Sprache',
    skip: 'Zum Inhalt springen',
  },
  footer: {
    about: 'Eine Engine für Ansible-Playbooks, geschrieben in Rust. Open Source unter GPL-3.0-or-later.',
    use: 'Nutzen',
    reference: 'Referenz',
    project: 'Projekt',
    source: 'Quellcode',
    installation: 'Installation',
    quickstart: 'Schnellstart',
    willItRun: 'Läuft mein Playbook?',
    whenItFails: 'Wenn ein Lauf fehlschlägt',
    leaves: 'Was Volant auf Ihren Rechnern hinterlässt',
    uninstall: 'Deinstallation',
    cli: 'Befehlszeile',
    configuration: 'Konfiguration',
    matrix: 'Support-Matrix',
    changelog: 'Changelog',
    roadmap: 'Roadmap',
    contributing: 'Mitwirken',
    security: 'Sicherheitsrichtlinie',
    license: 'Lizenz',
    telemetry: 'Volant erhebt keine Telemetrie.',
    name: '<i>Volant</i> ist das französische Wort für Lenkrad.',
  },
  copy: { copy: 'Kopieren', copied: 'Kopiert', manual: 'Auswählen und kopieren', label: 'Befehl kopieren' },
  lookup: {
    label: 'Wird es unterstützt? Modul, Collection, Filter oder Option nachschlagen',
    button: 'Nachschlagen',
  },
  home: {
    title: 'Volant: Ihre Ansible-Playbooks, ausgeführt von einer schnellen Engine in Rust',
    description:
      'Volant liest die Playbooks, Roles und Inventories, die Sie schon haben, und führt sie per SSH auf Linux-Hosts aus, deutlich schneller als ansible-playbook.',
    heading: 'Ihre Ansible-Playbooks, ausgeführt von einer schnellen Engine in&nbsp;Rust.',
    lede: 'Volant liest die Playbooks, Roles und Inventories, die Sie schon haben, und führt sie per SSH aus. Nichts muss umgestellt werden.',
    platforms: 'Linux und macOS, x86_64 und arm64. Ansible wird nicht benötigt. Windows wird noch nicht unterstützt.',
    quickstart: 'Schnellstart',
    source: 'Quellcode auf GitHub',
    badge: 'Pre-Alpha',
    status:
      'Es führt echte Playbooks aus, aber noch nicht jedes. Probieren Sie es auf einem Host aus, an dem Sie Änderungen verschmerzen können, und lesen Sie, <a class="text-link" href="/docs/#what-volant-leaves-on-your-machines">was es auf Ihren Rechnern hinterlässt</a>.',
    runTitle: 'Ein Lauf, fünf Hosts.',
    runText:
      'Ein echter Lauf von Volant, noch einmal abgespielt. Jede Speiche des Rads steht für einen Host und nimmt die Farbe seines Ergebnisses an, in den Farben von ansible-playbook.',
    howTitle: 'So funktioniert es.',
    howText:
      'ansible-playbook bezahlt für jeden Task einen neuen Python-Interpreter auf dem Host. Volant bezahlt einmal pro Lauf und hält danach eine Verbindung offen.',
    everyTask: 'bei jedem Task, auf jedem Host',
    slowSteps: ['Modul-Payload bauen', 'per SSH senden', 'Python starten', 'module_utils importieren', 'Modul ausführen'],
    oncePerRun: 'einmal pro Lauf',
    onceSteps: ['einen kleinen Agent hochladen, auf dem Host zwischengespeichert', 'eine SSH-Verbindung pro Host öffnen'],
    then: 'danach',
    forEveryTask: 'bei jedem Task',
    fastSteps: ['den Task über die offene Verbindung senden', 'in Rust beantworten oder ein warmes Python forken'],
    howNote:
      'Gängige Module wie <code>apt</code>, <code>systemd</code>, <code>file</code>, <code>lineinfile</code> und das Sammeln von Facts beantwortet der Agent in Rust, wenn die Argumente eines Tasks es zulassen. Jedes andere Modul läuft in Python, in einem Server, der seine Imports einmal geladen hat. <a class="text-link" href="/docs/internals/architecture/">Wie die Engine aufgebaut ist</a>',
    filesTitle: 'Dieselben Dateien, derselbe Befehl.',
    filesText:
      '<code>volant playbook</code> übernimmt die Optionen von <code>ansible-playbook</code> und gibt die gewohnte Ausgabe aus, bis hin zum Recap. Alles, was Ihr Projekt schon enthält, wird unverändert gelesen.',
    filesLink: 'Ihr erstes Playbook ausführen',
    files: [
      ['ansible.cfg', 'von denselben Orten gelesen, mit derselben Rangfolge'],
      ['Inventories', 'INI, YAML, TOML, Skripte und Inventory-Plugins'],
      ['group_vars und host_vars', 'in der Reihenfolge von Ansible geschichtet'],
      ['Vault', 'verschlüsselte Dateien und Werte, --ask-vault-pass und --vault-id'],
      ['Roles und Collections', 'Abhängigkeiten, Argument-Specs und installierte Collections'],
      ['Befehlszeile', 'die Optionen von ansible-playbook, einschließlich --check und --diff'],
    ],
    refusalTitle: 'Es sagt Nein, bevor es einen Host anfasst.',
    refusalText:
      'Volant prüft das ganze Playbook, bevor es sich verbindet. Das meiste, was diese Version nicht ausführen kann, wird dort benannt, mit dem Play oder Task, in dem es steht, und der Lauf endet mit Code 4. Auf keinem Host hat sich etwas geändert.',
    refusalCheck:
      '<code>--syntax-check</code> und die Auflistungen halten nie bei Nicht-Unterstütztem an, und <code>--check</code> mit <code>--diff</code> zeigt, was ein echter Lauf ändern würde. <a class="text-link" href="/docs/start/compatibility/">Ein Projekt prüfen</a>',
    refusalCaption: 'Echte Ausgabe von Volant 0.1.0-alpha.10 für ein Play, das eine Strategie verlangt, die diese Version nicht hat.',
    runtime: [
      ['Was es herunterlädt', 'ein eigenes Python und ansible-core, etwa 155 MB, beim ersten Mal, wenn ein Playbook Python braucht'],
      ['Wie es prüft', 'jede Datei gegen einen im Release festgelegten Hash'],
      ['Wo es liegt', '<code>~/.local/share/volant/runtime</code>, getrennt von jedem System-Python'],
      ['Collections', '<code>volant runtime install --collections community.general</code>'],
      ['Ohne Netzwerk', '<code>volant runtime pack</code> auf der einen Maschine, <code>install --from</code> auf der anderen'],
    ],
    runtimeTitle: 'Kein Ansible zu installieren.',
    runtimeText:
      'Das Binary enthält die Agents, die es auf Ihre Hosts hochlädt. Braucht ein Playbook ein Python-Modul, installiert Volant eine eigene Runtime und hält sie vom Rest der Maschine getrennt.',
    runtimeLink: 'Die verwaltete Runtime',
    faster: 'schneller bei {workload}, gemessen gegen ansible-core mit Pipelining',
    coreShare: 'der {total} Keywords, Optionen und Plugins von ansible-core laufen heute',
    blogTitle: 'Aus dem Blog.',
    blogAll: 'Alle Beiträge',
    promisesTitle: 'Was Volant verspricht.',
    promises: [
      ['Ihre Dateien bleiben Ihre', 'Kein Konvertierungsschritt und kein neues Format. Dieselben Dateien funktionieren weiter mit ansible-playbook.'],
      ['Ablehnungen kommen zuerst', 'Das meiste, was diese Version nicht ausführen kann, wird vor der ersten Verbindung benannt, und es läuft nichts.'],
      ['Zuerst Linux über SSH', 'Heute Linux-Hosts über SSH und lokale Verbindungen. Windows und Netzwerkgeräte folgen später.'],
      ['Keine Telemetrie', 'Volant macht keine Netzwerkaufrufe außer denen, die Ihr Playbook verlangt, und dem Download der Runtime.'],
    ],
    openTitle: 'Offen entwickelt.',
    openText:
      'Volant steht unter GPL-3.0-or-later, wie Ansible. Die Roadmap, die Issues und die Entscheidungen hinter dem Design sind öffentlich.',
    involved: 'Mitmachen',
  },
  performance: {
    title: 'Ansible-Performance: Volant im Vergleich mit ansible-core 2.19',
    description:
      'Zwei reale Workloads, gemessen gegen ansible-core 2.19.12 mit Pipelining, die zugrunde liegende Konfiguration und die Befehle, mit denen sich die Messungen reproduzieren lassen.',
    heading: 'Gemessen an echten Playbooks.',
    lede: 'Zwei Workloads, jeder von einem Rezept im Repository ausgeführt, gegen ansible-core 2.19.12 mit aktiviertem Pipelining. Jede Zahl auf dieser Seite stammt aus diesen Rezepten, gemessen am {date}.',
    faster: 'schneller bei {workload}',
    workloads: {
      roles: {
        name: 'Vier Galaxy-Roles',
        inSentence: 'vier Galaxy-Roles',
        detail: 'geerlingguy.security, nginx, git und pip auf zwei bereits konvergierten Ubuntu-24.04-Hosts, 56 Tasks pro Host',
      },
      k3s: {
        name: 'k3s-ansible site.yml',
        inSentence: 'k3s-ansible site.yml',
        detail: 'das offizielle k3s-ansible-Playbook gegen einen installierten Cluster, ein Server und ein Agent',
      },
    },
    ratios:
      'Die Faktoren liegen aus gutem Grund weit auseinander. Die Roles ändern auf einem konvergierten Host nichts, daher geht fast die gesamte Zeit der Referenz in die Mechanik um jedes Modul. Das k3s-Playbook startet bei jedem Durchlauf Dienste neu und führt Installationsskripte aus, und diese Arbeit dauert mit beiden Engines gleich lang.',
    splitTitle: 'Wo Volant seine Zeit verbringt.',
    splitText: 'Der mediane Durchlauf, nach Schritten aufgeteilt. Ein nativer Schritt wird vom Agent auf dem Host in Rust beantwortet.',
    k3sServer: 'k3s-ansible, der Server',
    forTwoHosts: '{time} für zwei Hosts',
    steps: {
      connect: 'Verbindungen öffnen',
      files: 'file ×8, lineinfile ×8, stat ×5',
      systemd: 'systemd, 3 Tasks',
      setup: 'setup, Facts sammeln',
      apt: 'apt, 8 Tasks',
      wire: 'Frames und Blobs auf der Leitung',
      prepare: 'Tasks auf dem Controller vorbereiten',
      moduleSet: 'Python-Modulsatz bauen',
      wait: 'Warten auf den anderen Host',
      restart: 'k3s neu starten, Server und Agent',
      serviceFacts: 'service_facts',
      aptCache: 'apt mit update_cache',
      packageFacts: 'package_facts',
      facts: 'Facts sammeln',
    },
    kinds: { native: 'nativ', nativeSystemd: 'natives systemd', cached: 'zwischengespeichert', python: 'Python' },
    k3sWork:
      'Bei k3s ist der größte Teil der 23.5 Sekunden Arbeit, die das Playbook verlangt: den k3s-Dienst neu starten, sein Installationsskript ausführen und 13 <code>command</code>-Tasks. Beide Engines zahlen dafür gleich viel, weil es Prozesse auf dem Host sind. <a class="text-link" href="/docs/project/performance/#what-is-left">Was übrig bleibt</a> erklärt den Rest.',
    configTitle: 'Die Konfiguration.',
    configText: 'Auf beiden Seiten Standardeinstellungen, außer Pipelining, das die Referenz schneller macht.',
    config: [
      ['Verwaltete Hosts', 'zwei Ubuntu-24.04-Hosts, je 4 Kerne'],
      ['Controller', 'Ubuntu 26.04, 8 Kerne'],
      ['Volant', 'Release-Build, Standardeinstellungen: native Module an, eine SSH-Verbindung pro Host'],
      ['Referenz', 'ansible-core 2.19.12 mit pipelining = True, Forks auf dem Standardwert 5'],
      ['Durchläufe', 'ein Aufwärmdurchlauf pro Engine, danach je drei gemessene Durchläufe; berichtet wird der Median'],
      ['Prüfung', 'jeder Durchlauf muss bei beiden Engines mit demselben PLAY RECAP enden, sonst schlägt das Rezept fehl'],
    ],
    ownTitle: 'Messen Sie Ihren eigenen Lauf.',
    ownProfile: 'Bei Ihrem eigenen Playbook gibt <code>--profile</code> die obige Aufteilung für Ihren Lauf aus, Schritt für Schritt.',
    ownRecipes:
      'Um die beiden Workloads zu reproduzieren, bauen die Rezepte Volant im Release-Modus, laufen gegen die Hosts, die Sie nennen, und geben die Mediane und das Verhältnis aus. Sie brauchen ansible-core 2.19.12, installiert mit <code>uv tool install</code>, und zwei Hosts, auf denen das Konto <code>sudo</code> ohne Passwort hat. Die Rezepte verändern diese Hosts, nehmen Sie also Maschinen, die Sie neu aufsetzen können. <a class="text-link" href="/docs/project/performance/#reproducing-the-figures">Die Rezepte</a>',
    ownRun: '# Your own run, split by step',
    ownBench: '# The two workloads of this page',
    caveat:
      'Diese Zahlen beschreiben zwei Playbooks auf zwei Hosts. Ihre hängen von Ihrem Netzwerk ab, davon, was Ihre Tasks tun, wie viele davon ein natives Modul abdeckt und wie viele Hosts Sie gleichzeitig ansteuern. <a class="text-link" href="/docs/project/performance/">Die vollständige Messung</a>',
  },
  compatibility: {
    title: 'Ansible-Kompatibilität: was Volant heute ausführt',
    description:
      'Wie viel von ansible-core 2.19.12 und vom ansible-Paket Volant heute ausführt, Name für Name von der Testsuite gemessen.',
    heading: 'Kompatibel mit ansible-core 2.19, Name für Name.',
    lede: 'Volant lädt die gesamte Grammatik von ansible-core 2.19 und reproduziert dessen Verhalten, daher ist jede Abweichung ein Bug. Die Testsuite listet jedes Keyword, jede Option und jedes Plugin auf, das ansible-core mitbringt, und prüft, was diese Version damit tut.',
    coreRuns: 'von ansible-core laufen: {runs} von {total} Namen, {partial} weitere teilweise',
    collectionsRun:
      'der Module und Plugins der Collections: {runs} von {total}. Die meisten der {later} übrigen kommen mit der Unterstützung für Windows und Netzwerkgeräte.',
    also: 'Ein Projekt prüfen? <a class="text-link" href="/docs/start/compatibility/">Läuft mein Playbook?</a> zeigt die drei Befehle, und die <a class="text-link" href="#known-gaps-note">bekannten Lücken</a> listen auf, was ohne Meldung fehlschlägt.',
    runs: 'läuft',
    partialLegend: 'teilweise: ein Argument oder ein Wert fehlt',
    laterLegend: 'später: Windows, andere Verbindungen, andere Output-Callbacks',
    source: 'Die Zahlen stammen aus der Support-Matrix, die die Testsuite erzeugt.',
    groups: {
      language: 'Die Playbook-Sprache',
      modules: 'Module und Templating',
      hosts: 'Hosts, Läufe und Befehlszeile',
      collections: 'Collections im ansible-Paket',
    },
    partial: '{n} teilweise',
    later: '{n} später',
    checkTitle: 'Prüfen Sie Ihr eigenes Projekt.',
    checkText: 'Drei Befehle, und die ersten beiden berühren nie einen Host.',
    steps: {
      read: {
        title: 'Lesen, ohne auszuführen',
        note: 'Auflistungen und --syntax-check halten nie bei Nicht-Unterstütztem an, daher funktionieren sie mit jedem Playbook.',
      },
      list: {
        title: 'Die Task-Liste vergleichen',
        note: 'Stimmt sie mit der Ausgabe von ansible-playbook --list-tasks überein, hat Volant Ihre Roles, Imports und Tags genauso kompiliert.',
      },
      limit: {
        title: 'Auf einem Host ausführen',
        note: 'Das meiste, was diese Version nicht ausführen kann, stoppt den Lauf vor der Verbindung, nennt, was abgelehnt wurde, und endet mit Code 4.',
      },
    },
    gaps: 'Einige Lücken sind still: Ein Lauf kann dort eine Einstellung ohne Fehler ignorieren. <a class="text-link" href="/docs/start/compatibility/#known-gaps">Lesen Sie die bekannten Lücken</a> vor einem Lauf, auf den es ankommt.',
  },
  community: {
    description:
      'Wie Sie eine Abweichung von Ansible melden, ein Issue übernehmen, eine Frage stellen oder zu Volant beitragen, und was die nächsten Releases bringen.',
    heading: 'Offen entwickelt.',
    lede: 'Der Code, die Roadmap, die Issues und die Entscheidungen hinter dem Design sind öffentlich. Bugreports, Fixes, Features und Dokumentation sind willkommen.',
    ways: {
      failed: {
        title: 'Mein Lauf ist fehlgeschlagen',
        text: 'Lesen Sie den Exit-Code und die Meldung, grenzen Sie den Fehler auf einen Task ein und vergleichen Sie mit ansible-playbook. Die Seite führt Sie Schritt für Schritt durch.',
        action: 'Wenn ein Lauf fehlschlägt',
      },
      difference: {
        title: 'Eine Abweichung melden',
        text: 'Ein Playbook, das sich unter Volant anders verhält als unter ansible-playbook, ist ein Bug. Die Vorlage fragt ab, was zum Reproduzieren nötig ist.',
        action: 'Kompatibilitätsmeldung öffnen',
      },
      issue: {
        title: 'Ein Issue übernehmen',
        text: 'Jedes geplante Issue nennt das erwartete Verhalten, die Akzeptanzkriterien und den Einstieg im Code. Kommentieren Sie es, bevor Sie anfangen.',
        action: 'Good First Issues',
      },
      ask: {
        title: 'Fragen oder vorschlagen',
        text: 'Fragen, Ideen und Design-Diskussionen gehören in die Discussions. Bei allem, was über einen Fix hinausgeht, eröffnen Sie zuerst ein Issue, damit der Ansatz abgestimmt ist.',
        action: 'Zu den Discussions',
      },
      docs: {
        title: 'Die Dokumentation verbessern',
        text: 'Jede Seite hat einen Link zum Bearbeiten und einen zum Melden eines Problems. Die Dokumentation liegt im selben Repository wie die Engine.',
        action: 'Dokumentation lesen',
      },
    },
    setupTitle: 'In vier Befehlen eingerichtet.',
    setupText:
      'Das Repository legt seine Rust-Toolchain fest, daher wählt rustup die richtige Version. Jede Änderung läuft über einen Pull Request, mit Tests für das, was sie berührt.',
    setupLink: 'CONTRIBUTING.md lesen',
    setupHooks: 'Git-Hooks und die Tools, die die Rezepte nutzen',
    setupCheck: 'Formatierung, Lints und die Testsuite',
    nextTitle: 'Was als Nächstes kommt.',
    nextText: 'Die bisher geplanten Releases in der Reihenfolge. Jedes ist ein Meilenstein auf GitHub mit seinen Issues.',
    nextLink: 'Die Roadmap',
    license: 'Lizenz',
    licenseText:
      'GPL-3.0-or-later, wie Ansible. <a class="text-link" href="/docs/decisions/0001-gpl-3-0-or-later-license/">Entscheidungsdokument 0001</a> erklärt, warum.',
    security: 'Sicherheit',
    securityText:
      'Melden Sie eine Schwachstelle vertraulich, nie in einem öffentlichen Issue. <a class="text-link" href="{repo}/blob/main/SECURITY.md">Die Sicherheitsrichtlinie</a> erklärt, wie.',
    support: 'Support',
    supportText:
      'Wo Sie fragen können und was Sie angeben sollten, damit jemand helfen kann. <a class="text-link" href="{repo}/blob/main/SUPPORT.md">SUPPORT.md</a>',
    decisions: 'Entscheidungen',
    decisionsText:
      'Die Entscheidungen, die prägen, was Nutzer sehen, sind mit ihren Gründen festgehalten. <a class="text-link" href="/docs/decisions/">Die Entscheidungsdokumente</a>',
  },
  console: {
    hosts: 'Hosts des Laufs',
    output: 'Ausgabe des Laufs',
    source: 'Das Playbook und das Inventory dieses Laufs',
    provenance:
      'Aufgezeichnet mit {version} auf fünf Hosts über eine lokale Verbindung und hier in Lesegeschwindigkeit abgespielt. Die Ausgabe ist das, was gedruckt wurde.',
    states: { waiting: 'wartet', ok: 'ok', changed: 'changed', skipped: 'skipped', failed: 'failed' },
  },
  traces: {
    label: '{workload}: ansible-core läuft {ansible} Sekunden, Volant {volant} Sekunden',
    hint: 'Fahren Sie über die Kurve, um sie zu lesen.',
    running: 'läuft',
    done: 'fertig nach {seconds} s',
    caption:
      'Median von drei gemessenen Durchläufen am {date}, gegen {reference} mit pipelining = True. Eine Linie ist hoch, solange der Lauf dieser Engine aktiv ist.',
  },
  docs: {
    report: 'Ein Problem mit dieser Seite melden',
    staleTitle: 'Diese Übersetzung ist möglicherweise veraltet',
    staleBody: 'Die englische Seite wurde nach der Übersetzung dieser Seite geändert.',
    staleLink: 'Die englische Seite lesen',
  },
};
