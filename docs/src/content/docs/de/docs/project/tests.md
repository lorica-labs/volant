---
sourceHash: 2e7bc4a7539f
title: Tests
description: Wie Volant gegen ansible-core geprüft wird, die Arten aufgezeichneter Antworten und wie Sie ein Szenario hinzufügen.
---

Volant muss sich wie ansible-core 2.19.12 verhalten. Der größte Teil der Testsuite ist deshalb ein Vergleich: ansible-core hat einmal etwas ausgeführt, seine Antwort wurde im Repository aufgezeichnet, und der Test führt Volant mit derselben Eingabe aus und vergleicht.

## Die fünf Arten

Jede Art von Aufzeichnung hat einen Namen, und `just golden <kind>` erzeugt sie neu.

| Art | Was aufgezeichnet wurde | Wo sie läuft |
|---|---|---|
| `template` | Das Ergebnis beim Rendern jedes Ausdrucks in `crates/volant/tests/golden/cases.yml` | In jedem Job, mit `reference.rs` |
| `inventory` | Hosts, Variablen und Gruppen eines Inventorys sowie die Hosts, die jedes Muster auswählt | In jedem Job, mit `reference.rs` |
| `scenario` | Ein Playbook-Lauf: der Exit-Code und die gesamte Standardausgabe | In jedem Job, mit `reference.rs` |
| `module` | Was jedes native Modul auf einem echten Rechner geantwortet hat | Im Job `ssh` und mit `just test-reference`, mit `reference_modules.rs` |
| `command-names` | Die Ausgabe der Skripte in `tests/command-names`, die `ansible`, `ansible-playbook`, `ansible-vault`, `ansible-inventory`, `ansible-doc`, `ansible-config` und `ansible-galaxy` aufrufen | Der Job `command-names` |

Nichts in einer Aufzeichnung wird von Hand bearbeitet. Ändern Sie die Eingabe, führen Sie `just golden <kind>` mit installiertem ansible-core 2.19.12 aus und lesen Sie den Diff. Auf einem unveränderten Baum ausgeführt, lässt es die Dateien genau so, wie sie waren. Die Art `module` braucht außerdem `sudo`, systemd, apt und cron, weil sie auf dem Rechner ein Konto und eine Unit anlegt.

## Der Job command-names

Der Job `command-names` läuft in einem Container `debian:stable-slim` ohne `ansible` und ohne Python. Er verlinkt die Namen `ansible-*` mit `volant shims install` auf das statische `volant` und führt dann die Skripte aus `tests/command-names` genau so aus, wie ein Benutzer sie für ansible schreiben würde. Jedes Skript schreibt seine Ausgabe in eine Datei und vergleicht sie mit `tests/command-names/expected/<script>.out`, einer Aufzeichnung von ansible-core 2.19.12, die `just golden command-names` erstellt hat. Der Job lädt die Ausgabedateien hoch, wenn ein Skript fehlschlägt.

## Ein Szenario hinzufügen

Ein Szenario ist ein Verzeichnis unter `crates/volant/tests/scenarios`:

1. Legen Sie `crates/volant/tests/scenarios/<name>/` an und legen Sie darin das Playbook, das Inventory und alle Dateien ab, die das Playbook liest.
2. Schreiben Sie `case.yml` daneben. Ein einzelner Lauf ist `args: [-i, inv.ini, site.yml]`. Mehrere Läufe desselben Verzeichnisses sind `runs:`, eine Liste von `{args: [...]}`.
3. Führen Sie `just golden scenario` aus. Es schreibt `expected.json` ins Verzeichnis.
4. Führen Sie `touch crates/volant/tests/reference.rs` und danach die Tests aus. Die Liste der Szenarien steht fest, wenn diese Datei kompiliert wird, sodass ein neues Verzeichnis erst nach einem Neubau erkannt wird.

Jeder Volant-Lauf geschieht in einer frischen Kopie des Verzeichnisses, mit `--forks 1`, sofern der Fall nichts anderes sagt. Der Test vergleicht den Exit-Code und die vollständige Standardausgabe, mit maskierten Zeiten. Außer `args` und `runs` nimmt `case.yml` sechs weitere Schlüssel:

- `modes` listet `check`, `check-diff` und `diff` auf. Jeder Lauf geschieht dann pro Modus einmal mehr, mit `--check`, `--check --diff` oder `--diff` zusätzlich, in dieser festen Reihenfolge, und jeder Lauf wird aufgezeichnet.
- `env` setzt Umgebungsvariablen für den Lauf.
- `program: ansible` zeichnet die Läufe mit `ansible` statt `ansible-playbook` auf und führt `volant adhoc` statt `volant playbook` aus.
- `forks` erhöht die Zahl der Forks über eins. Hosts geben dann möglicherweise in anderer Reihenfolge aus, führen Sie also die Zeilen, deren Reihenfolge abweichen darf, als reguläre Ausdrücke in `unordered` auf. `unordered` ohne `forks` wird abgelehnt.
- `needs` nennt, was der Rechner haben muss: `ansible-core`, `sudo` oder `systemd`. Ein Fall, dessen Voraussetzungen nicht erfüllt sind, wird übersprungen, außer wenn `VOLANT_PYTHON` gesetzt ist, was das Überspringen in einen Fehlschlag verwandelt.
- `links` ordnet einem Pfad das Ziel eines Links zu, den jede Kopie unter diesem Pfad erhält. Nutzen Sie es für einen Link, den git oder cargo nicht halten können, etwa einen Link auf eine Datei, die nicht existiert: `cargo package` lehnt einen versionierten Link dieser Art ab.

## Die Paritätsmatrix

`crates/volant/tests/parity.rs` vergleicht jeden Namen, den ansible-core 2.19.12 und die Collections des festgelegten `ansible`-Pakets kennen (Keywords, Module, Filter, Tests, Lookups und die anderen Plugin-Arten), mit dem, was die Engine damit macht. `just parity-reference` erzeugt die Referenzseite neu, `crates/volant/tests/parity/reference.json`. Die Seiten [Parität](/de/docs/reference/parity/), [Keywords](/de/docs/reference/keywords/) und [Module](/de/docs/reference/modules/) werden von `just docs-parity`, `just docs-keywords` und `just docs-modules` aus den eigenen Tabellen der Engine erzeugt, und ein Test schlägt fehl, wenn eine davon veraltet ist.

## Ausführen

`just check-touched` ist die Prüfung, die Sie vor einem Pull Request ausführen. Sie formatiert, führt clippy und die Ratsche für Dateigrößen aus, führt die Tests der Module aus, die der Branch ändert, und prüft die Rechtschreibung. `just check-local` tut dasselbe mit jedem Test. CI führt jeden Test aus.

`just test` führt alles aus, was keinen Host braucht. `just test-reference` führt die Modulaufzeichnungen auf dem Rechner aus, auf dem Sie sind, was ansible-core, sudo und systemd braucht. `just ssh-test` ist das, was der Job `ssh` ausführt: Er startet von einem sshd auf localhost aus und deckt auch die End-to-End-Tests ab. Bevorzugen Sie ein Szenario oder eine Tabellenzeile in einem vorhandenen Test gegenüber einer neuen Testfunktion.
