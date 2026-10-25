---
sourceHash: 7b06690a9fe4
title: "Ansible nur durch das erweitern, was sich zurückführen lässt"
description: "Entscheidungsdokument 0011."
sidebar:
  label: "0011 · Erweiterungen und Eject"
  order: 11
---

| Status | Datum |
|---|---|
| vorgeschlagen | 2026-10-25 |

## Kontext

Volant führt Ansible-Playbooks so aus, wie es ansible-core 2.19 tut ([Entscheidung 0002](/de/docs/decisions/0002-ansible-core-2-19-as-reference/)). Sobald das gilt, kann es anbieten, was Ansible fehlt. Das Risiko ist ein Projekt, das von einer solchen Funktion abhängig wird und nicht mehr zu ansible-core zurückkann. Ein Team übernimmt Volant leichter, wenn es weiß, dass es jederzeit wieder gehen kann.

Zwei Dinge machen das Zurückgehen möglich. YAML bleibt die Quelle: Nichts ersetzt es. Und jede Erweiterung hat eine Regel, die sie in einfaches Ansible mit demselben Verhalten umschreibt, angewendet von einem Befehl.

## Entscheidung

### Zwei Arten von Erweiterungen

Inhalte, die Ansible laden kann, etwa Filter, Lookups und Module, kommen in die Collection `volant.compat`. Sie funktionieren unter ansible-core unverändert und müssen nie zurückgeführt werden.

Schlüsselwörter und Syntax, die nur Volant versteht, werden als `volant.<schlüsselwort>` geschrieben, als Schlüssel des Plays, Blocks oder Tasks, für den sie gelten:

```yaml
- name: one account per user
  volant.loop: "{{ users }}"
  block:
    - ansible.builtin.user:
        name: "{{ item }}"
    - ansible.posix.authorized_key:
        user: "{{ item }}"
        key: "{{ lookup('file', 'keys/' ~ item ~ '.pub') }}"
```

Ein Schlüssel statt eines `volant:`-Mappings, weil jede Erweiterung dann ein Schlüssel ist, nach dem man suchen kann und der für sich umgeschrieben wird. ansible-core lehnt einen solchen Schlüssel laut ab, gemessen mit 2.19.12: `[ERROR]: 'volant.loop' is not a valid attribute for a Block`, mit der Zeile. Ein Playbook mit einer Erweiterung läuft unter ansible-core also nie halb verstanden.

### Der Vertrag der Umschreibung

Jede Erweiterung kommt mit einer Regel, die sie in einfaches Ansible umschreibt. Das Verhalten der Erweiterung ist als das Verhalten ihrer Umschreibung unter ansible-core 2.19.12 festgelegt: Volant erfindet keine Semantik, es führt die umgeschriebene Form schneller aus und hält das Original lesbar.

Jede Erweiterung kommt außerdem mit einem Hin-und-zurück-Test. Er führt die umgeschriebene Form unter ansible-core 2.19.12 und die Erweiterung unter Volant aus und vergleicht Ausgabe und Zusammenfassung. Keine Erweiterung wird ohne beides gemergt.

### `volant eject`

```sh
volant eject [PATH...] [--out DIR] [--json]
```

Der Befehl liest die Playbooks und Rollen unter `PATH` (standardmäßig das aktuelle Verzeichnis) und schreibt jede Datei, die eine Erweiterung verwendet, mit allen Erweiterungen umgeschrieben. Mit `--out DIR` wird der Baum dorthin geschrieben, ohne wird an Ort und Stelle umgeschrieben. Die Umschreibung kann Dateien hinzufügen, etwa die eingebundene Task-Datei einer umgeschriebenen Schleife. Sie kommen unter `_volant/` neben die Datei, aus der sie stammen, benannt nach dem Task und einem Hash seines Inhalts, sodass zweimaliges Zurückführen denselben Baum ergibt.

Eine Erweiterung, deren Umschreibung in einem bestimmten Kontext nicht exakt ist, etwa `run_once` unter der Strategie `free` weiter unten, wird mit `Datei:Zeile` und Grund abgelehnt, und nichts wird geschrieben. Der Bericht listet jede umgeschriebene und jede hinzugefügte Datei. `--json` gibt ein Objekt pro Datei aus: `path`, `action` (`rewritten` oder `added`) und die umgeschriebenen Erweiterungen. Die Exit-Codes sind 0, wenn alles umgeschrieben wurde, 1, wenn eine Erweiterung abgelehnt wurde, und 4, wenn eine Datei nicht gelesen werden kann.

### `volant check --portable`

```sh
volant check --portable [PATH...] [--json]
```

Der Befehl listet jede Zeile, die ansible-core ablehnen würde, mit `Datei:Zeile` und der verwendeten Erweiterung. Er endet mit 0, wenn es keine gibt, sonst mit 1. `--json` gibt ein Objekt pro Zeile aus: `path`, `line`, `extension`. Ein CI-Job kann damit eine Schranke setzen, und die Ausgabe von `volant eject` muss sie passieren.

### Die ersten Erweiterungen

Sie werden nach den öffentlichen Stimmen im Issue-Tracker von ansible-core ausgewählt.

1. **Eine Schleife über einen Block** (ansible/ansible#13262), `volant.loop` mit `volant.loop_control` an einem Block. Die Umschreibung verschiebt die Tasks des Blocks in eine eingebundene Datei und läuft in einer Schleife über das Einbinden:

   ```yaml
   - name: one account per user
     ansible.builtin.include_tasks: _volant/one-account-per-user-3f2a.yml
     loop: "{{ users }}"
   ```

   Gemessen mit 2.19.12, zwei Hosts, unter `linear` und unter `serial: 1`: Die Einbindungen werden zuerst aufgelöst, dann laufen die Tasks jedes Elements nacheinander, jeder Task über alle Hosts. Ein `when` darin gilt pro Element, und `run_once` läuft einmal pro Element. Unter der Strategie `free` beachtet ansible-core `run_once` in der eingebundenen Datei nicht: Es warnt und führt den Task auf jedem Host aus. Ein `run_once` in einem `volant.loop` unter `free` wird daher von `eject` abgelehnt und von `check --portable` gemeldet, da Volant der Umschreibung folgt.

2. **Ein Block als Handler** (ansible/ansible#14270): ein Block in `handlers:`, den ein Task über den Namen des Blocks benachrichtigen kann. Gemessen mit 2.19.12 schlägt das Benachrichtigen eines Blocks über seinen Namen fehl mit `[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list`. Die Umschreibung macht aus dem Block einen Handler, der seine Tasks einbindet:

   ```yaml
   handlers:
     - name: restart app
       ansible.builtin.include_tasks: _volant/restart-app-91c0.yml
   ```

   Gemessen mit 2.19.12: Der benachrichtigte Handler bindet die Datei für jeden benachrichtigten Host ein und führt ihre Tasks der Reihe nach aus, wie es ein Block tun würde. Er braucht keinen neuen Schlüssel, denn Volant akzeptiert den Block dort, wo ansible-core ihn nicht nutzen kann. Er wird also als einfaches Ansible geschrieben und trotzdem umgeschrieben.

3. **`collection uninstall`** (ansible/ansible#67759), ein Befehl von `volant galaxy` statt einer Syntax. Er ändert nichts an einem Playbook, braucht also keine Umschreibung und blockiert nie ein Zurückführen.

Die übrigen Wünsche bleiben späteren Dokumenten überlassen: `until` und `async` an einem Block (#46203, #28997) haben keine exakte Umschreibung, die Versuche und Warten erhält, und ein bedingtes `import_playbook` (#34281), eine Ausführlichkeit pro Task (#24215), UND und ODER in Tag-Ausdrücken (#11396) sowie eine Liste von Adressen für `ansible_host` (proposals#97) brauchen jeweils zuerst eine eigene Messung.

### Shadow-Wiederholungen und das Lock

Eine [Shadow-Wiederholung](/de/docs/start/switch-without-risk/) wiederholt eine Aufzeichnung von ansible-core, und ansible-core lehnt eine Erweiterung ab, also erscheint eine Erweiterung nie in einer Aufzeichnung. Ein Projekt, das beides will, hält seine Playbooks portabel, solange es aufzeichnet und wiederholt, und nutzt Erweiterungen erst nach dem Umstieg. Alternativ zeichnet es die Ausgabe von `volant eject` auf und wiederholt diese. Das Lock folgt derselben Regel: Es legt die Engine fest, die ein Projekt ausführt, und sagt nichts darüber, welche Erweiterungen das Projekt verwendet.

## Konsequenzen

- Ein Projekt kann Volant jederzeit verlassen: `volant eject` gibt ihm einen Baum zurück, den ansible-core ausführt, und `check --portable` beweist es.
- Jede Erweiterung kostet eine Umschreibungsregel und einen Hin-und-zurück-Test, bevor sie gemergt wird, und ein Kontext, in dem die Umschreibung nicht exakt ist, wird abgelehnt, nicht angenähert.
- Die Umsetzung folgt in diesen Issues, nach Größe:
  - die `volant.`-Schlüssel lesen und einen unbekannten ablehnen (klein);
  - der Umschreibungsdurchlauf, geteilt von `eject` und den Hin-und-zurück-Tests (mittel);
  - `volant eject` (mittel);
  - `volant check --portable` (klein);
  - `volant.loop` und `volant.loop_control` an einem Block (mittel);
  - ein Block als Handler (klein);
  - `volant galaxy collection uninstall` (klein).

## Messungen, 2026-10-25

ansible-core 2.19.12, zwei Hosts über eine lokale Verbindung, `gather_facts: false`. Die umgeschriebene Schleife bindet drei Tasks ein, `create`, `once` (mit `run_once: true`) und `key` (mit `when: item != 'bob' or inventory_hostname == 'h1'`), über `[alice, bob]`.

Unter `linear` kommen zuerst die Einbindungen, dann die Tasks jedes Elements nacheinander:

```text
TASK [per user] ****************************************************************
included: _volant/per-user.yml for h1, h2 => (item=alice)
included: _volant/per-user.yml for h1, h2 => (item=bob)
TASK [create] ******************************************************************
ok: [h1] => {"msg": "create alice on h1"}
ok: [h2] => {"msg": "create alice on h2"}
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key alice"}
ok: [h2] => {"msg": "key alice"}
TASK [create] ******************************************************************
...
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key bob"}
skipping: [h2]
```

Unter `free` wird `run_once` nicht beachtet:

```text
[WARNING]: Using run_once with the free strategy is not currently supported. This task will still be executed for every host in the inventory list.
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [once] ********************************************************************
ok: [h2] => {"msg": "once alice"}
```

Unter `serial: 1` führt jeder Batch die ganze Schleife aus, Host für Host. Ein Block in `handlers:` lässt sich nicht über seinen Namen benachrichtigen, und seine Umschreibung läuft:

```text
TASK [change] ******************************************************************
[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list

RUNNING HANDLER [restart app] **************************************************
included: _volant/restart-app.yml for h1, h2
RUNNING HANDLER [stop] *********************************************************
ok: [h1] => {"msg": "stop"}
ok: [h2] => {"msg": "stop"}
RUNNING HANDLER [start] ********************************************************
ok: [h1] => {"msg": "start"}
ok: [h2] => {"msg": "start"}
```
