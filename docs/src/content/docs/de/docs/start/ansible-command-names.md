---
sourceHash: b74cf526136c
title: Ansible-Befehlsnamen
description: Welcher Volant-Befehl welches Programm von ansible-core ersetzt, was gleich ist, was abweicht und wie Sie Ihre Kommandozeilen behalten.
---

Volant führt die neun Programme von ansible-core 2.19.12 aus. Jedes ist ein Unterbefehl von `volant`, und unter Linux und macOS kann es auch unter seinem ansible-Namen gestartet werden. Windows wird weder für die Programmnamen noch für die Links unterstützt.

## Was was ersetzt

| Programm von ansible-core | Befehl von Volant | Läuft in | Verwaltete Runtime |
|---|---|---|---|
| `ansible` | [`volant adhoc`](/de/docs/reference/cli/adhoc/) | Volant | für die nativen Module nicht nötig |
| `ansible-playbook` | [`volant playbook`](/de/docs/reference/cli/), `volant-playbook` | Volant | bei der ersten Verwendung installiert, die Python braucht |
| `ansible-vault` | [`volant vault`](/de/docs/reference/cli/vault/) | Volant | nicht nötig |
| `ansible-inventory` | [`volant inventory`](/de/docs/reference/cli/inventory/) | Volant | nicht nötig, außer für Inventory-Plugins von Collections |
| `ansible-doc` | [`volant doc`](/de/docs/reference/cli/doc/) | dem eigenen Werkzeug von ansible-core | bei der ersten Verwendung installiert |
| `ansible-config` | [`volant config`](/de/docs/reference/cli/config/) | dem eigenen Werkzeug von ansible-core | bei der ersten Verwendung installiert |
| `ansible-galaxy` | [`volant galaxy`](/de/docs/reference/cli/galaxy/) | dem eigenen Werkzeug von ansible-core | bei der ersten Verwendung installiert |
| `ansible-pull` | [`volant pull`](/de/docs/reference/cli/pull/) | Volant | für einen `git`-Checkout nicht nötig |
| `ansible-console` | [`volant console`](/de/docs/reference/cli/console/) | dem eigenen Werkzeug von ansible-core | bei der ersten Verwendung installiert |

Die verwaltete Runtime installiert sich beim ersten Mal selbst, wenn ein Befehl sie braucht, wie bei einem Playbook, und gibt dabei eine Zeile auf der Standardfehlerausgabe aus. [Die verwaltete Runtime](/de/docs/reference/runtime/) beschreibt sie. Sie müssen ansible nicht installieren, aber vier der neun Befehle führen den eigenen Code von ansible-core aus dieser Runtime aus. `volant console` führt Tasks mit dem Executor von ansible-core aus, nicht mit dem von Volant.

## Ihre Kommandozeilen behalten

Die Links werden nur angelegt, wenn Sie darum bitten. Führen Sie entweder das Installationsskript mit `--shims` aus oder [`volant shims install`](/de/docs/reference/cli/shims/):

```sh
curl -fsSL https://volant.sh/install.sh | sh -s -- --shims
volant shims install
```

Jeder Name ist ein symbolischer Link auf `volant`. Volant liest den Dateinamen, unter dem es gestartet wurde, sodass der Link `ansible-vault` `volant vault` ausführt. Beide Befehle nehmen dieselben Argumente und geben dieselbe Ausgabe und dieselben Exit-Codes. Ein Aufruffehler endet mit 2 und nennt das Programm unter dem gestarteten Namen, `ansible-vault: error: ...`.

`install` ersetzt keine Datei und keinen Link, die es nicht angelegt hat, außer mit `--force`. Liegt ein anderes Programm dieses Namens in Ihrem `PATH`, sagt es das: `note: /usr/bin/ansible-vault is hidden by this link, DIR comes first in PATH`, wenn der Link gewinnt, oder `note: ... still runs first, DIR comes later in PATH`, wenn nicht. So entfernen Sie die Links:

```sh
volant shims remove
```

`remove` löscht nur Links, die auf eine Datei namens `volant` zeigen, und lässt alles andere.

Ein CI-Skript wie dieses läuft unverändert, sobald die Links existieren, ohne ansible auf dem Rechner:

```sh
ansible-inventory -i hosts.ini --list > inventory.json
ansible-vault view group_vars/all/secrets.yml --vault-password-file ci-pass
ansible-playbook -i hosts.ini site.yml
```

## Was gleich ist

Bei `ansible`, `ansible-vault` und `ansible-inventory` wurden die Ausgabe, die auf den Befehlsseiten aufgeführten Optionen und die Exit-Codes gegen ansible-core 2.19.12 gemessen. Hinter jeder Aussage steht ein Test.

| Aussage | Wo sie geprüft wird |
|---|---|
| `volant adhoc` gibt die Bytes des Callbacks `minimal`, den `[ERROR]`-Block und die Formen `-o` und `-t` aus, mit den Exit-Codes der Referenz | die Szenario-Goldens `adhoc-*` und `crates/volant/tests/adhoc_cli.rs` |
| Der Name `ansible` führt `volant adhoc` aus | `the_ansible_name_runs_adhoc` |
| Der Name `ansible-playbook` führt `volant playbook` aus | `a_link_named_ansible_playbook_behaves_as_the_playbook_subcommand` |
| `volant vault` liefert die Meldungen, Exit-Codes, Dateimodi und Dateien von ansible-vault | `crates/volant/tests/vault_cli.rs` und die Szenario-Goldens `vault-*` für die Vault-Daten, die ein Playbook liest |
| Eine Datei, die das eine Werkzeug verschlüsselt, entschlüsselt das andere, für die Standard-ID und für ein Label | `a_file_one_tool_encrypts_the_other_decrypts` |
| `volant inventory` gibt die JSON- und YAML-Bytes von ansible-inventory aus: Schlüsselreihenfolge, Einrückung von vier Leerzeichen, PyYAML-Anker | `crates/volant/tests/inventory_golden.rs` über `golden/inventories/cases.yml` und `inventory_cli.rs` |
| `volant config` und `volant galaxy` geben aus, was das Werkzeug ausgibt | `crates/volant/tests/tools_cli.rs` |
| `volant pull` gibt den Playbook-Lauf und die Zeile von `-o` wie `ansible-pull` aus und checkt ohne Python aus | `a_pull_prints_what_the_reference_pull_prints` und `a_pull_checks_out_and_runs_without_python` in `console_pull.rs` |
| `volant console` liest seine Befehle wie `ansible-console` aus einer Pipe | `the_console_reads_its_commands_from_a_pipe` |
| Die Links, die `install` anlegt, die Zeilen, die es ausgibt, und was `remove` stehen lässt | `crates/volant/tests/shims.rs` |
| Welche Namen auf welchen Befehl abgebildet werden, und kein anderer | die Tests in `crates/volant/src/cli/program.rs` |

Die Optionen, die jeder native Befehl akzeptiert, stehen auf seiner eigenen Seite: [`volant adhoc`](/de/docs/reference/cli/adhoc/), [`volant vault`](/de/docs/reference/cli/vault/), [`volant inventory`](/de/docs/reference/cli/inventory/) und [`volant pull`](/de/docs/reference/cli/pull/). Eine Option, die dort nicht steht, wird abgelehnt, nie ignoriert.

## Was abweicht

- Der Hilfetext ist der von Volant. Er wird nie mit der Referenz verglichen.
- `--version` gibt `ansible [core 2.19.12]` (oder den Programmnamen) und die Pfade aus. Die Zeilen zu Python, Jinja und PyYAML entfallen, wenn Volant keinen Interpreter mit ansible-core findet, weil `--version` nichts installiert. Siehe [`volant adhoc`](/de/docs/reference/cli/adhoc/).
- `executable location` in `--version` ist das Programm, wie es gestartet wurde: `volant` oder der Link. Siehe [`volant doc`](/de/docs/reference/cli/doc/#terminal-exit-code-und---version).
- `ansible-inventory --toml` braucht kein Python-Paket, wo die Referenz `tomli-w` braucht, und ein `null`-Wert ergibt eine `[ERROR]`-Zeile statt eines Tracebacks (beide enden mit 250). Siehe [`volant inventory`](/de/docs/reference/cli/inventory/).
- `volant pull` gibt seine ersten zwei Zeilen vor dem aus, was seine Schritte ausgeben, und sein Checkout startet kein Python. Siehe [`volant pull`](/de/docs/reference/cli/pull/#unterschiede-zu-ansible-pull).
- `volant config dump` zeigt `COLLECTIONS_PATHS` als von der Umgebung gesetzt, sobald die Runtime ein Verzeichnis `collections/` hat. Siehe [`volant config`](/de/docs/reference/cli/config/#der-collections-pfad).
- `ansible-vault view` läuft nur in einem Terminal über einen Pager, mit `PAGER`, dann `ANSIBLE_PAGER`, dann `[defaults] pager`, dann `less`. Siehe [Vault](/de/docs/variables/vault/).
- `volant adhoc` und `volant playbook` nutzen Volants Engine, daher gelten die Unterschiede, die in [Läuft mein Playbook?](/de/docs/start/compatibility/) und auf [`volant adhoc`](/de/docs/reference/cli/adhoc/#aktionen-die-keine-module-sind) aufgeführt sind.

## Exit-Codes

| Befehl | 0 | 1 | 2 | 4 | 5 | 250 |
|---|---|---|---|---|---|---|
| `volant adhoc` | jeder Host war erfolgreich oder wurde übersprungen | kein Host mehr als Ziel, oder eine falsche `meta`-Aktion | ein Host ist fehlgeschlagen, oder ein Aufruffehler | ein Host war nicht erreichbar, oder etwas, das diese Version noch nicht unterstützt | eine ungültige Einstellung, oder kein Argument für `command` | ein Argument, das kein gültiges JSON ist |
| `volant vault` | erledigt | ein Dateifehler, etwa `create` auf eine vorhandene Datei | ein Aufruffehler | | eine falsche Option, etwa zwei Dateien mit `--output` | |
| `volant inventory` | ausgegeben | | ein Aufruffehler | | keine Aktion oder zwei Aktionen, oder ein falsches Ziel von `--host` oder `--graph` | |

`volant playbook` verwendet die Codes auf [Exit-Codes](/de/docs/reference/exit-codes/). Die vier Werkzeuge, die in der Runtime von ansible-core laufen, liefern den eigenen Code des Werkzeugs, oder 1 mit einer Zeile `[ERROR]:`, wenn sich der Interpreter nicht starten lässt. `volant pull` endet mit 2, wenn der Checkout fehlschlägt, mit 5 bei einem Optionsfehler oder wenn kein Playbook gefunden wird, und sonst mit dem Code des Playbooks.
