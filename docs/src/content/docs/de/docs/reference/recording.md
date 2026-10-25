---
sourceHash: 0c8e4bf84fac
title: Aufzeichnungen von Läufen
description: Die Datei, die der Callback volant.compat.record während eines Laufs von ansible-playbook schreibt, Zeile für Zeile.
---

Bevor ein Team umsteigt, kann es aufzeichnen, was `ansible-playbook` bei seinen echten Läufen tut, und diese Läufe später von Volant wiederholen lassen. Die Aufzeichnung schreibt der Callback `volant.compat.record`. Er wird in der Collection `volant.compat` ausgeliefert, braucht nur die Python-Standardbibliothek und läuft mit ansible-core 2.19 und neuer.

## Aktivieren

Installieren Sie die Collection dort, wo ansible-core seine Collections findet, und fügen Sie `ansible.cfg` zwei Abschnitte hinzu:

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

Es ist ein Benachrichtigungs-Callback. Er ersetzt nie den Stdout-Callback, und Ausgabe und Exit-Code des Laufs sind dieselben wie ohne ihn.

| Einstellung | Bedeutung |
|---|---|
| `path` | Die zu schreibende Datei. `{{ timestamp }}` wird zur Startzeit des Laufs in UTC, im Format `20261025T093000Z`; kein anderes Template wird gelesen. Ein Name, der auf `.gz` endet, wird mit gzip komprimiert. Fehlende übergeordnete Verzeichnisse werden mit Modus `0700` angelegt. Der Standardwert ist `shadow/run-{{ timestamp }}.jsonl.gz`. |

`VOLANT_RECORD_PATH` setzt `path` aus der Umgebung, was zu einem CI-Job passt.

Kann die Datei nicht geschrieben werden, gibt der Callback eine Warnung aus, und der Lauf geht ohne Aufzeichnung weiter.

## Sicherheit

Eine Aufzeichnung enthält die Task-Ergebnisse so, wie ansible-core sie gesehen hat. Sie kann entschlüsselte Vault-Werte, die Extra-Variablen des Laufs und alles, was ein Modul zurückgegeben hat, enthalten. Behandeln Sie sie als Geheimnis:

- Die Datei wird mit Modus `0600` angelegt, und eine vorhandene Datei wird auf `0600` zurückgesetzt.
- Halten Sie das Verzeichnis der Aufzeichnungen aus der Versionsverwaltung heraus, und speichern Sie Aufzeichnungen aus der CI als private Artefakte mit kurzer Aufbewahrungsdauer.
- Ein Task mit `no_log` behält seinen Status, und sein Ergebnis wird als `{"censored": true}` aufgezeichnet.

## Format

Eine Aufzeichnung ist JSON Lines: ein JSON-Objekt pro Zeile, jedes mit einem Schlüssel `event`. Diese Seite beschreibt Format `1`.

| `event` | Felder |
|---|---|
| `header` | `format`, `ansible_core` (seine Version), `cli` (die Kommandozeilenoptionen des Laufs, wie ansible-core sie gelesen hat), `cwd`, `playbooks` (`path` und `sha256` jedes Playbooks), `inventory` (die `sources` wie angegeben sowie `path` und `sha256` jeder Quelle, die eine Datei ist), `extra_vars_hash`, `started` |
| `inventory` | `hosts`: jeder Host des Inventars mit den Gruppen, zu denen er außer `all` gehört |
| `play` | `uuid`, `name`, `hosts` (das Muster des Plays) |
| `task` | `uuid`, `path` (`Datei:Zeile`), `name`, `action` (der vollständige Name des Moduls oder der Aktion), `handler`, `role` |
| `result` | `host`, `task` (seine `uuid`), `item`, `status`, `delegated_to`, `result` |
| `retry` | `host`, `task`, `attempt`, `result`: ein fehlgeschlagener Versuch einer `until`-Schleife |
| `include` | `file`, `hosts`: eine Datei, die `include_tasks` oder `include_role` hinzugefügt hat, und die Hosts, die sie ausführen |
| `notify` | `handler`, `host`: ein Handler, der auf einem Host laufen wird |
| `stats` | `hosts` (die Zusammenfassung jedes Hosts), `custom` (was `set_stats` gesammelt hat) |

Die Zeilen `header` und `inventory` kommen zuerst, wenn das erste Play beginnt. `stats` ist die letzte Zeile eines vollständigen Laufs, eine Aufzeichnung ohne sie stammt also von einem unterbrochenen Lauf.

Hinweise zu den Feldern:

- Jeder Hash ist SHA-256 in Hexadezimal. `extra_vars_hash` umfasst jedes `-e`-Argument wie angegeben, gefolgt von einem NUL-Byte, und bei einem `@Datei`-Argument den Inhalt der Datei, gefolgt von einem weiteren NUL-Byte. Eine Wiederholung übergibt dieselben Argumente, der Hash ändert sich also nur, wenn sich eine genannte Datei ändert.
- `status` ist `ok`, `changed`, `failed`, `skipped` oder `unreachable`. Ein Fehler, den `ignore_errors` durchgelassen hat, bleibt `failed`.
- In einer Schleife hat jedes Element eine `result`-Zeile, deren `item` sein Index ab 0 ist. Die folgende Zeile mit `item` gleich `null` ist der ganze Task, mit den Elementen unter `results`.
- `delegated_to` ist der Host, den ein `delegate_to` genannt hat, oder `null`.
- `result` ist das Ergebnis des Tasks, wie Callbacks es erhalten, einschließlich der Schlüssel, die ansible-core hinzufügt, etwa `invocation`, `false_condition`, `skip_reason` und `attempts`. Welche davon vorhanden sind, hängt von der Version ab: ansible-core 2.21 gibt `invocation` nicht mehr an Callbacks weiter.
- Das Ergebnis der Fact-Erfassung wird vollständig aufgezeichnet, weil eine Wiederholung Templates damit rendert. Das sind 50 bis 100 KB pro Host, deshalb ist gzip der Standard.
- `include_tasks` erzeugt eine eigene `task`-Zeile und ein `result` pro Host, dann eine `include`-Zeile pro Datei. `import_tasks` erzeugt keines von beiden, nur die importierten Tasks.
- `notify`-Zeilen werden geschrieben, wenn die Handler laufen, nach den Tasks, die sie benachrichtigt haben.
