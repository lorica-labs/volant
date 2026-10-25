---
sourceHash: 8577d9c13ab0
title: Rangfolge und Facts
description: Welche Quelle gewinnt, wenn zwei dieselbe Variable definieren, die magischen Variablen und wie Facts gesammelt, benannt und zwischengespeichert werden.
---

Diese Seite beantwortet zwei Fragen: welchen Wert eine Variable hat, wenn mehrere Quellen sie setzen, und woher die Facts über einen Host stammen.

Definieren zwei Quellen dieselbe Variable, gewinnt die, die in dieser Liste weiter unten steht. Die Reihenfolge ist die von ansible-core 2.19, wobei jede Rollenschicht gegen ihre Nachbarn gemessen wurde.

1. Das `defaults/main.yml` einer Rolle
2. `group_vars/all`, aus dem Inventory-Verzeichnis, dann aus dem Playbook-Verzeichnis
3. Gruppenvariablen des Inventorys, angewendet nach Gruppentiefe, dann nach Name
4. `group_vars/<group>`, aus dem Inventory-Verzeichnis, dann aus dem Playbook-Verzeichnis
5. Hostvariablen des Inventorys
6. `host_vars/<host>`, aus dem Inventory-Verzeichnis, dann aus dem Playbook-Verzeichnis
7. Vom Host gesammelte Facts
8. `vars` des Plays
9. `vars_files` des Plays
10. Das `vars/main.yml` einer Rolle
11. `vars` eines Tasks
12. `set_fact`, registrierte Ergebnisse und `include_vars`
13. Rollenparameter und die `vars:` einer dynamischen Include-Anweisung
14. `--extra-vars`

[Rollen](/de/docs/playbooks/roles/#rollenvariablen) erklärt, woher die drei Rollenschichten kommen und wie sie sich unterscheiden.

Eine Vars-Datei, die nur `---` und Kommentare enthält, wird wie in ansible-core als leeres Mapping geladen. Eine Datei, die einen Skalar wie `42` enthält, wird weiterhin abgelehnt.

Ein Mapping kann die Schlüssel eines anderen mit einem Merge-Key übernehmen, `<<: *base`, in jeder YAML-Datei, die Volant liest: Variablendateien, Inventories, Playbooks und Task-Argumente. Die zusammengeführten Schlüssel kommen zuerst, und ein Schlüssel, der neben `<<` steht, ersetzt den zusammengeführten. In `<<: [*a, *b]` gewinnt das erste Mapping. Ein Wert, der weder ein Mapping noch eine Liste von Mappings ist, bricht das Laden ab, mit der Meldung von PyYAML.

:::note
Ein gesammelter Fact unterliegt den eigenen Variablen des Plays. Ein Playbook, das eine Variable mit demselben Namen wie ein Fact setzt, liest seinen eigenen Wert, wie in Ansible.
:::

## Magische Variablen

Eine feste Menge magischer Variablen liegt über der Sicht jedes Hosts und gewinnt immer:

| Variable | Enthält |
|---|---|
| `inventory_hostname`, `inventory_hostname_short` | Den Inventory-Namen dieses Hosts. |
| `group_names`, `groups` | Die Gruppen dieses Hosts und jede Gruppe mit ihren Hosts. |
| `hostvars` | Die Variablen jedes Hosts, siehe unten. |
| `ansible_play_hosts`, `ansible_play_hosts_all` | Die aktiven Hosts des Plays und alle seine Hosts. |
| `ansible_play_batch`, `play_hosts` | Die Hosts des aktuellen [`serial`-Batches](/de/docs/playbooks/how-a-play-runs/#serial-batches). `play_hosts` ist veraltet. |
| `playbook_dir`, `inventory_dir`, `inventory_file` | Pfade des aktuellen Laufs. |
| `omit` | Der Platzhalter, der ein Modulargument weglässt. |
| `vars` | Alle Variablen dieses Hosts, nach Namen gelesen. `vars['a']`, `vars[name]` und `vars.get('a')` rendern ein Template, das die Variable enthält, wie `{{ a }}`. `omit` gehört nicht dazu. |
| `ansible_playbook_python` | Der Python-Interpreter, der ansible-core für Volant ausführt. Wird gefunden, wenn ein Template ihn liest, und ist undefiniert, wenn keiner gefunden wird. |
| `ansible_check_mode`, `ansible_diff_mode` | Ob der Lauf `--check` und `--diff` hat. Ein eigenes Keyword `check_mode` oder `diff` an einem Play oder Task ändert sie nicht. |
| `ansible_run_tags`, `ansible_skip_tags` | Die Tags, die der Lauf auswählt und überspringt, siehe [Check-Modus, Diff und Auflistungen](/de/docs/playbooks/check-and-diff/#ansible_run_tags-und-ansible_skip_tags). |
| `ansible_forks` | Der aktuelle Wert von `forks`. |
| `ansible_version` | Das Release von ansible-core, das Volant nachbildet, siehe [Entscheidungsdokument 0003](/de/docs/decisions/0003-ansible-version-reports-the-reference-release/). |
| `volant_version` | Die eigene Version von Volant. Prüfen Sie `volant_version is defined`, um die beiden Engines zu unterscheiden. |

In einem `rescue` werden `ansible_failed_task` und `ansible_failed_result` als Facts auf dem Host gesetzt, der fehlgeschlagen ist.

Jeder Task erhält außerdem `ansible_search_path`, und der Task einer Rolle erhält `role_path`. Beide sind vorhanden, bevor die eigenen `vars:` des Tasks aufgelöst werden, sodass eine Task-Variable, die mit `lookup('template')` gebildet wird, das `templates/` der Rolle findet. [Templating](/de/docs/variables/templating/#was-es-gibt) listet auf, was der Suchpfad enthält.

## hostvars

`hostvars` enthält, was Inventory, `--extra-vars`, `set_fact` und gesammelte Facts für jeden Host erzeugt haben. Es enthält nicht die Play- oder Task-Variablen eines anderen Hosts. Es wird neu aufgebaut, sobald sich ein Fact ändert.

Jeder Host liest eine gemeinsame Map statt einer eigenen Kopie, sodass ein Task, der `hostvars` nennt, einen Zugriff kostet und keine Kopie des Inventorys.

Ein Task, der `hostvars`, `ansible_play_hosts` oder `ansible_play_batch` liest, wartet, bis jeder Host ihn erreicht hat, und sieht daher, was Ansible an dieser Stelle zeigen würde.

## Facts

Facts sind das, was ein Host über sich selbst meldet, etwa seinen Namen, seine Distribution und seine Adressen.

```yaml
- hosts: web
  tasks:
    - debug:
        msg: "{{ ansible_hostname }} runs {{ ansible_facts.distribution }}"
```

Gesammelte Facts stehen über den Inventory-Variablen und unter den eigenen Variablen des Plays, wie die Liste am Anfang dieser Seite zeigt.

### Wann Facts gesammelt werden

Ein Play sammelt Facts, es sei denn, es setzt `gather_facts: false`. Mit `gathering = explicit` in `ansible.cfg` sammelt nur ein Play, das `gather_facts` schreibt. Mit `smart` wird ein Host übersprungen, der frische Facts im [Fact-Cache](/de/docs/hosts/inventories/#der-fact-cache) hat. Ein Template in `gather_facts` wird einmal gerendert, wenn das Play startet.

Volant führt entweder das eigene Modul `setup` von ansible-core über den [warmen Python-Pfad](/de/docs/internals/python/) aus oder lässt den Agent (das kleine Programm, das Volant auf jeden Host legt) die Facts selbst sammeln, wenn das Playbook nachweislich nichts anderes liest. Siehe [Native Facts](#native-facts).

Weil `setup` immer auf sein Python-Modul zurückfallen kann, braucht der Controller ansible-core und der Host einen Python-3-Interpreter. Ein Lauf, der Python nur zum Sammeln von Facts braucht, installiert die [verwaltete Runtime](/de/docs/reference/runtime/) nicht. Ein Controller ohne nutzbares ansible-core hält daher ein Play, das Facts sammelt, vor der ersten Verbindung an, statt ohne sie weiterzumachen. Setzen Sie `gather_facts: false` bei Plays, die keine Facts brauchen.

`gather_subset` und `gather_timeout` funktionieren an einem Play und am Modul `setup` mit `filter`, wie in ansible-core.

### Zwei Namen für jeden Fact

Jeder Fact ist an zwei Stellen lesbar:

- flach, mit dem Präfix `ansible_`: `ansible_hostname`
- innerhalb von `ansible_facts`, so geschrieben, wie das Modul ihn zurückgegeben hat: `ansible_facts.hostname`

Es sind getrennte Variablen. Ein späteres `set_fact` von `ansible_hostname` überdeckt die flache und lässt `ansible_facts.hostname` unberührt. Ansible verhält sich genauso.

### Native Facts

Ein nativer Fact-Collector ist eine in Rust geschriebene Version, die im Agent läuft, sodass der Host dafür kein Python braucht. Er existiert für Debian und Ubuntu. Er sammelt die 17 Collectors, die ansible-core für `gather_subset: min` ausführt, die Prozessor- und Speicher-Facts des Collectors `hardware` von ansible-core sowie die Standardrouten und Adresslisten seines Collectors `network`. Ein Fact, den der native Collector nicht erzeugt, etwa Mounts, Geräte oder Details einzelner Schnittstellen, wird ausgelassen.

Ein fehlender Fact ist nur wichtig, wenn etwas ihn liest. Daher liest Volant vor der ersten Verbindung jedes Play, jede Rolle, jedes Template und jede Variablendefinition des Laufs und listet die Facts auf, die sie lesen können. Es nutzt den nativen Collector nur, wenn alle davon Facts sind, die er erzeugt. Ein Zugriff über einen berechneten Namen, etwa `ansible_facts[item]`, `vars['ansible_' ~ name]`, `lookup('vars', ...)` oder ein ganzes `hostvars[host]`, das an einen Filter übergeben wird, könnte jeden Fact erreichen, daher bleibt es beim `setup` von ansible-core.

Der native Collector gibt den Task auch auf einem Host zurück, den er nicht nachbilden kann: Er gibt auf, und das Python-Modul `setup` läuft. Das geschieht auf einer anderen Distribution, mit aktiviertem SELinux, mit lokalen Facts in `/etc/ansible/facts.d` oder bei einem Hostnamen, den nur DNS kennt.

`--facts python` führt immer das Modul von ansible-core aus, und `--facts native` fragt immer den Agent. `--profile` gibt aus, welches der Lauf gewählt hat und warum. Siehe [Kommandozeile](/de/docs/reference/cli/#lauf-und-ausgabe).

### Facts sind Daten

Facts sind die eigenen Worte des Hosts, sie kommen also nicht vertrauenswürdig an: Ein Template innerhalb eines Facts bleibt Text und wird nie gerendert. Siehe [Werte von Hosts werden nie als Template gerendert](/de/docs/variables/templating/#werte-von-hosts-werden-nie-als-template-gerendert).

### Facts zwischen Läufen behalten

Mit `fact_caching = jsonfile` bleiben Facts von einem Lauf zum nächsten erhalten, siehe [Der Fact-Cache](/de/docs/hosts/inventories/#der-fact-cache).
