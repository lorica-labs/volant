---
sourceHash: 480ffa167c92
title: Schleifen und Wiederholungen
description: loop, with_items und loop_control sowie wie until, retries und delay einen Task wiederholen.
---

Diese Seite beantwortet, wie Sie einen Task mit `loop` und `with_<lookup>` über eine Liste von Elementen wiederholen und wie Sie einen Task mit `until`, `retries` und `delay` so lange erneut ausführen, bis er gelingt.

## Schleifen

```yaml
- name: Create the service accounts
  command: useradd --system {{ item }}
  loop:
    - app
    - worker
  loop_control:
    label: "{{ item }}"
  register: accounts
```

`loop` und `with_items` können nicht beide am selben Task stehen. `with_items` glättet eine Ebene verschachtelter Listen, `loop` nicht, wie in Ansible. Registriert man einen Task mit Schleife, entsteht eine Liste `results` mit einem Eintrag pro Element.

`loop_control` liest `loop_var`, `label`, `index_var`, `pause`, `extended`, `extended_allitems` und `break_when`. Ein Unterschlüssel, den ansible-core nicht kennt, wird beim Laden des Playbooks abgelehnt. Jede Form `with_<lookup>` läuft. Siehe [Keywords](/de/docs/reference/keywords/#under-loop_control) und [`with_`-Schleifen](#with_-schleifen).

## `with_`-Schleifen

`with_<name>` ist `loop: "{{ lookup('<name>', *terms, wantlist=True) }}"`, und es läuft für jedes Lookup-Plugin, das ansible-core hat:

| Lookup | Beispiele |
|---|---|
| Die fünfundzwanzig aus `ansible.builtin` | `with_dict`, `with_nested`, `with_subelements`, `with_sequence` |
| Lookups, die ansible-core nach `community.general` umleitet | `with_flattened`, `with_cartesian`, `with_filetree` |
| Das Lookup einer Collection, unter seinem vollständigen Namen | `with_community.general.dependent` |
| Ein Plugin in einem Verzeichnis `lookup_plugins/` neben dem Playbook oder einer Rolle | der Dateiname des Plugins, als `with_<name>` |

Ein eingebautes Lookup wird auch als `with_ansible.builtin.<name>` und `with_ansible.legacy.<name>` akzeptiert.

Das ansible-core des Controllers sagt vor der ersten Verbindung, ob ein Name ein Lookup ist. Ein `with_`-Name, der keines ist, lässt das Playbook wie in Ansible mit `'with_xyz' is not a valid attribute for a Task` fehlschlagen. Ein Task mit zwei Schleifen schlägt mit `duplicate loop in task: <name>` fehl und ein `with_`-Schlüssel ohne Wert mit `you must specify a value when using with_<name>`.

Ein Lookup, das einen Fehler auslöst, lässt den Task mit dem eigenen Satz des Plugins fehlschlagen. Ein Lookup, das Volant selbst in Rust ausführt (`file`, `unvault`, `vars`, `pipe`, `template` und `first_found`), schlägt mit `The lookup plugin '<name>' failed: <reason>` und der Kette von ansible-core fehl, auch in einem `when:` und in einer Template-Datei. `vars` formuliert es als `No variable named '<name>' was found.`, ohne Plugin-Teil. Ein Schleifenwert, der ein einzelnes Template ist, das eine undefinierte Variable liest, schlägt mit `'name' is undefined` fehl. Eine Liste, deren Element eine undefinierte Variable liest, schlägt mit dem Satz der Engine dafür fehl, ohne die Kette, die Ansible ausgibt.

## Jedes Element rendern

Eine Schleife rendert die Argumente, das `when`, die Keywords und das Label eines Elements, wenn sie das Element erreicht, nicht schon vor dem Start der Schleife.

- Ein Element, das sich nicht rendern lässt, schlägt allein fehl, als `failed: [h] (item=2)`. Die Elemente danach laufen weiter, und der Task endet mit `One or more items failed`.
- Ein `break_when`, das die Schleife zuerst beendet, verbirgt die Render-Fehler der Elemente danach.
- Eine `debug`- oder `set_fact`-Schleife mit `loop_control.pause` rendert jedes Element nach der Pause, sodass `now()` die Zeit liest, zu der das Element läuft, und ein `set_fact` in einer Schleife gibt weiterhin an die folgenden Elemente weiter, was es gesetzt hat.
- Ein `loop_control.label`, das eine undefinierte Variable liest, lässt sein Element fehlschlagen, nachdem das Modul gelaufen ist.

## `with_first_found` und `with_fileglob`

```yaml
- name: Distribute the matching config
  copy:
    src: "{{ item }}"
    dest: /etc/app/config.yml
  with_first_found:
    - "{{ ansible_distribution }}.yml"
    - default.yml

- name: Copy every image
  copy:
    src: "{{ item }}"
    dest: /var/lib/app/images/
  with_fileglob:
    - "images/*.tar.gz"
```

Beide laufen über ein Lookup, wie jede Form `with_<name>`: `with_first_found` geht seine Terme durch, bis einer eine Datei nennt, die existiert, und `with_fileglob` sammelt jeden Treffer eines Glob-Musters. `with_first_found` durchsucht außerdem ein Unterverzeichnis jedes Sucheintrags vor dem Eintrag selbst: `templates/`, wenn die Action des Tasks `template` nennt, `vars/`, wenn sie `include_vars` nennt, sonst `files/`, dasselbe Verzeichnis, das ein einfacher Task mit `template:` oder `include_vars:` von sich aus durchsuchen würde.

Wird nichts gefunden, schlägt der Task mit dem eigenen Satz von ansible-core fehl, `No file was found when using first_found.`, es sei denn, der Term ist ein Mapping mit `skip: true`, dann läuft die Schleife leer. Ein Term, der undefiniert rendert, wird verworfen, statt das ganze Lookup fehlschlagen zu lassen, ebenso ein undefinierter Wert in der Liste, die ein als ein String geschriebener Schleifenwert ergibt. Dadurch kann eine Rolle einen Fact lesen, den noch kein Host gesetzt hat, und trotzdem ihr `when` erreichen. Ein Pfad, den eines der Lookups findet und der noch eine Template-Markierung enthält (`{{`, `{%` oder `{#`), wird namentlich abgelehnt, statt ein zweites Mal gerendert zu werden.

Ein Pfad, den ein Lookup auf dem Controller gefunden hat, ist der eigene Inhalt des Playbooks. Ein Element, das teilweise aus einem Host-Wert gebildet wird, etwa ein Term, der `ansible_distribution` liest, wird als Daten gebunden, genau wie ein Element aus einem `loop:` über einen registrierten Wert.

## Wiederholen mit until

```yaml
- name: Wait for the service to answer
  command: curl -fsS http://localhost:8080/health
  register: health
  until: health.rc == 0
  retries: 10
  delay: 3
```

`until` führt einen Task erneut aus, bis sein Ausdruck gilt.

- `retries` ist die Anzahl der Wiederholungen nach dem ersten Versuch, nicht die Gesamtzahl: Der Task läuft bis zu `retries + 1` Mal. Es ist 3, wenn nur `until` geschrieben ist, für bis zu vier Läufe.
- Ein `retries` unter 1 schaltet das Wiederholen ab: Der Task läuft einmal, und sein Ergebnis hat kein `attempts`.
- Volant wartet `delay` Sekunden, standardmäßig 5, nach jedem der ersten `retries` fehlgeschlagenen Läufe, nie nach dem letzten.
- Das Ergebnis trägt die Anzahl der tatsächlich ausgeführten Wiederholungen unter `attempts`, höchstens `retries`. Ein Task, dem die Wiederholungen ausgehen, ist fehlgeschlagen, auch wenn das Modul selbst erfolgreich war. Mit einem `failed_when` entscheidet stattdessen sein Ergebnis im letzten Lauf: `failed_when: false` lässt den Task bestehen, und das registrierte Ergebnis sagt weiterhin `failed: true`.
- `retries` ohne `until` wiederholt den Task, solange sein Ergebnis fehlgeschlagen ist.
- Ein Task, der von einem Action-Plugin getragen wird (`copy`, `dnf`, `package` ...), wiederholt bei jedem Versuch seine ganze Folge von Untertasks von vorn, mit einem frischen Plugin und jeder Datei erneut gesendet. `until` liest nur das Ergebnis des letzten Untertasks.

:::note
Drei Wiederholungen mit der Standardverzögerung verbringen fünfzehn Sekunden mit Warten zwischen den vier Läufen, wie in Ansible.
:::

Ein Task mit Schleife wiederholt jedes Element für sich und beendet die Versuche eines Elements, bevor er das nächste beginnt.

`until`, `retries` und `delay` werden bei `include_tasks` und `include_role` vor der ersten Verbindung abgelehnt. Ansible lehnt sie dort ebenfalls ab, wenn es das Playbook lädt. `include_vars` ist ein gewöhnliches Modul und wiederholt wie jeder andere Task.

## Unterschiede zu Ansible

- Ein `until`-Ausdruck, der sich nicht auswerten lässt, wird mit dem Präfix von ansible-core und der eigenen Meldung von Volant gemeldet, genauso wie ein fehlschlagendes `when`.
- `FAILED - RETRYING` für einen Task in einer Rolle zeigt den eigenen Namen des Tasks statt `role : name`.
- Ein `loop_control.label`, das aus einem anderen Grund als einer undefinierten Variable fehlschlägt, etwa ein Lookup, das einen Fehler auslöst, lässt den Task fehlschlagen, bevor ein Element läuft. ansible-core beendet den Task, nachdem dieses Element gelaufen ist.
