---
sourceHash: 9e8e2288a96a
title: "Die ansible-core-Version pro Projekt wählen und behalten, was Ansible entfernt"
description: "Entscheidungsdokument 0010."
sidebar:
  label: "0010 · Versionsprofile"
  order: 10
---

| Status | Datum |
|---|---|
| vorgeschlagen | 2026-10-25 |

## Kontext

Volant bildet ansible-core 2.19.12 nach ([Entscheidung 0002](/de/docs/decisions/0002-ansible-core-2-19-as-reference/)). Ansible entwickelt sich weiter: 2.20 und 2.21 sind erschienen, 2.22 ist im Release Candidate. Jede Version entfernt oder ändert Verhalten, auf das sich Playbooks verlassen. Manche Teams bleiben bei der Version, die sie heute einsetzen, andere wollen das neueste Verhalten, und eine einzige Referenz kann nicht beiden dienen.

Am 2026-10-25 wurde der Golden-Korpus unter ansible-core 2.20.10, 2.21.5 und 2.22.0rc1 neu aufgezeichnet und mit den Aufzeichnungen von 2.19.12 verglichen, nachdem Zeiten, Job-IDs und Prozess-IDs entfernt waren. Der Korpus umfasst Templates, Inventories und 355 Szenarien mit 1032 Läufen. Vierzehn Dateien unterscheiden sich unter 2.20, 76 unter 2.21 und 94 unter 2.22. Die meisten Änderungen sind klein, etwa ein zusätzlicher Schlüssel in einem Ergebnis oder ein neues Präfix vor einem Fehler, aber manche ändern einen Lauf: Ein Schleifenergebnis hat kein `skipped` mehr, daher bricht ein Playbook, das `r.skipped` liest, unter 2.21 mit Exit-Code 2 ab. Der [Anhang](#anhang-korpus-diff-2026-10-25) listet sie auf.

## Entscheidung

### Profile

Ein Profil ist die ansible-core-Version, deren Verhalten Volant nachbildet. Es gibt zwei: `2.19`, die Voreinstellung, die bleibt, was Volant heute tut, und `2.22`, das hinzukommt, sobald ansible-core 2.22.0 erschienen und sein Korpus aufgezeichnet ist. `latest` bezeichnet das neueste Profil, das Volant mitbringt.

Ab 2.22 kommt für jede ansible-core-Version ein Profil hinzu, in der Volant-Version, die seine Aufzeichnungen ausliefert, und `latest` wechselt dorthin. Ein Profil entfällt, wenn ansible-core die Unterstützung seiner Version beendet. Die Versionen zwischen 2.19 und 2.22 bekommen kein eigenes Profil: Ihre Änderungen gehören zu `2.22`.

Ein Projekt wählt sein Profil mit `[volant] core_version` in seiner `ansible.cfg` oder mit der Umgebungsvariable `VOLANT_CORE_VERSION`, die Vorrang vor der Datei hat. Der Wert ist eine Version (`2.19`, `2.22`) oder `latest`. Jeder andere Wert beendet den Lauf mit Exit-Code 5 und listet die Profile auf. Wie `volant.lock` das Profil festhält, wird zusammen mit dem Lock in #653 entschieden.

`ansible_version` meldet die Version des Profils: 2.19.12 unter `2.19` und unter `2.22` das Patch-Release, mit dem der Korpus von `2.22` aufgezeichnet wurde. Das erweitert [Entscheidung 0003](/de/docs/decisions/0003-ansible-version-reports-the-reference-release/). `volant_version` ändert sich nicht.

Python-Module stammen aus dem ansible-core, das auf dem Controller installiert ist ([Entscheidung 0006](/de/docs/decisions/0006-warm-python-path/)), daher folgen ihre Ergebnisse dieser Version, unabhängig vom Profil. Volant warnt zu Beginn eines Laufs, wenn die installierte Version vom Profil abweicht.

### Schalter pro Verhalten

Es gibt eine Engine. Jeder Unterschied zwischen Versionen ist ein benannter Schalter mit der Version, ab der er gilt, etwa `loop-results-omit-skipped` ab 2.21, und der Code fragt das Profil, ob der Schalter aktiv ist. Jeder Schalter hat einen gleichnamigen Eintrag in der Registry unten.

Getrennte Verhaltenssätze wurden verworfen: Die gemessenen Unterschiede sind klein und über die Engine verteilt, eine Kopie pro Version würde also fast alles verdoppeln, um hier einen Schlüssel und dort eine Meldung zu ändern.

Der Code von Modulen und Action-Plugins wird nie geforkt. Ein natives Modul oder ein Action-Plugin hat eine einzige Implementierung, und ein Unterschied zwischen Versionen ist ein Schalter darin. Ein Schalter wird durch ein Szenario getestet, das unter jeder Version aufgezeichnet ist, die er ändert, und der Golden-Test führt jedes Szenario unter jedem Profil gegen die Aufzeichnung dieses Profils aus.

### Der Korpus pro Profil

`just golden` nimmt die Version als Argument und zeichnet mit einem ansible-core dieser Version auf, das in einer eigenen `uv`-Umgebung installiert ist. Die Aufzeichnungen von `2.19` bleiben, wo sie sind. Ein anderes Profil schreibt `expected.<version>.json` neben `expected.json`, nur für die Szenarien, deren Ergebnis abweicht: 94 von 355 unter 2.22.

Das Python des Controllers wird mit der Version festgelegt. Die Messung hat gezeigt, dass es die Ergebnisse ebenfalls ändert: Mit denselben Templates von 2.19 antworten Python 3.12 und 3.13 unterschiedlich auf `'abcabc'.replace('b', 'X', count=1)`, auf falsche Argumente an String-Methoden und auf das Sortieren einer Liste, die `nan` enthält.

Das Aufzeichnen dauert etwa 52 Minuten pro Version für die Goldens von Templates, Inventories und Szenarien auf einer Maschine mit 4 Kernen. Ein Maintainer macht es einmal, wie heute, und die CI zeichnet nie auf. Die CI vergleicht Volant mit den eingecheckten Aufzeichnungen, was einen zusätzlichen Lauf der Golden-Tests pro Profil kostet.

Ein Verhalten kommt nur mit einer Aufzeichnung aus dieser Version in ein Profil.

### Die Registry

Die Registry listet, was Ansible entfernt oder geändert hat, in `crates/volant/src/profile/registry.toml`. Jeder Eintrag hat einen `name` (den Schalter), das Feature (`feature`), die Version, in der es entfernt oder geändert wurde (`removed_in`), seine Art (`kind`, `security` oder `compat`), den Ersatz (`replacement`), die Warnung (`warning`) und für einen bekannten Upstream-Bug das Verhalten, dem Volant folgt (`follow`, `release` oder `documented`).

```toml
[[change]]
name = "debug-shows-registered-secret"
feature = "showing a registered secret with `debug`"
removed_in = "2.22"
kind = "security"
replacement = "write the value to a file with `copy` and `no_log: true`"
warning = "showing a registered secret with debug was removed in ansible-core 2.22"

[[change]]
name = "interpreter-auto-legacy"
feature = "`interpreter_python = auto_legacy` and `auto_legacy_silent`"
removed_in = "2.21"
kind = "compat"
replacement = "`auto` or `auto_silent`"
warning = "interpreter_python = auto_legacy was removed in ansible-core 2.21"

[[change]]
name = "loop-results-omit-skipped"
feature = "`skipped: false` in each loop result"
removed_in = "2.21"
kind = "compat"
replacement = "`r.skipped | default(false)`"
warning = "loop results lost their skipped key in ansible-core 2.21"
```

Unter einem Profil ab `removed_in` funktioniert ein `compat`-Feature weiter, mit der Warnung, gefolgt von „Volant still runs it“ und dem Ersatz. Ein `security`-Feature bleibt aus, außer bei `[volant] allow_insecure_legacy = true`. Unter einem älteren Profil ist das Feature einfach vorhanden, ohne Warnung.

Ein Eintrag ist `security`, wenn die Release Notes die Änderung mit einer CVE verbinden, wenn das entfernte Verhalten ein Geheimnis anzeigen kann oder wenn es Playbook-Inhalt erlaubt, außerhalb dessen zu lesen oder zu schreiben, was ihm übergeben wurde. Ein Sicherheitsfix, den ansible-core in einem 2.19-Patch-Release nach 2.19.12 ausliefert, gilt auch für das Profil `2.19`. Die bekannten `security`-Einträge sind:

- `debug`, das ein registriertes Geheimnis anzeigt, und `no_log`-Optionen, die in Ergebnissen durch einen Platzhalter ersetzt werden, beide in 2.22 geändert;
- das Maskieren von Geheimnissen mit `$REDACTED$` in jeder Ausgabe und in `log_path`, 2.22;
- `tempfile`, das einen Pfad in `prefix` und `suffix` annimmt, entfernt in 2.19.13, 2.20.9 und 2.21.4;
- Zugangsdaten, die `uri`, `get_url`, `apt_key`, `rpm_key` und der Lookup `url` in URLs anzeigen, maskiert ab 2.19.13, 2.20.9 und 2.21.4;
- das persistente Verzeichnis von ansible-connection ohne private Berechtigungen, behoben in 2.19.14, 2.20.10 und 2.21.5;
- `ansible-galaxy collection install`, das `FILES.json`-Einträge außerhalb der Collection annimmt (CVE-2026-89091), 2.22.0rc1.

Die übrigen Entfernungen der Porting-Guides sind `compat`, darunter `DEFAULT_TRANSPORT=smart` und der Parameter `vaultid` der `vault`-Filter (2.20), das Connection-Plugin `paramiko` und der Alias `gpg_whitelist` von `git` (2.21) sowie `yes` und `no` für `follow_redirects` (2.22).

### `volant upgrade-plan`

```sh
volant upgrade-plan [PATH...] [--to PROFILE] [--json]
```

Der Befehl liest die Playbooks, Rollen, Inventories und die `ansible.cfg` unter `PATH` (standardmäßig das aktuelle Verzeichnis) und listet jede Verwendung eines Features auf, das das Zielprofil (standardmäßig `latest`) gegenüber dem geltenden Profil entfernt oder ändert. Jeder Fund nennt `datei:zeile`, den Registry-Eintrag und die vorgeschlagene Änderung.

Manche Änderungen lassen sich durch Lesen nicht finden, etwa ein Ergebnis, das über einen berechneten Namen gelesen wird, oder der Text eines Fehlers. Der Bericht endet mit den Registry-Einträgen zwischen den beiden Profilen, die er nicht prüfen kann. Für diese zeichnet ein Projekt einen Lauf unter seiner aktuellen Version auf und spielt ihn mit [`volant shadow replay`](/de/docs/start/switch-without-risk/) unter dem Zielprofil ab. Die Abweichungen sind die Änderungen, auf die der Lauf treffen würde.

Die Exit-Codes sind 0, wenn nichts gefunden wurde, 1, wenn mindestens eine Verwendung gefunden wurde, 4, wenn eine Datei nicht gelesen werden kann, und 5 für ein unbekanntes Profil. `--json` gibt ein Objekt pro Fund aus: `path`, `line`, `name`, `removed_in`, `kind` und `suggestion`.

### Bekannte Regressionen

Volant folgt der Version, wenn eine Änderung gewollt ist. Es folgt dem dokumentierten Verhalten, wenn das Verhalten der Version ein offener Upstream-Bug ist, der auf einem Host das Falsche ausführt. Die Antwort fällt pro Fall und steht im Feld `follow` der Registry. Für die bekannten Regressionen von 2.21:

- Interpreter-Erkennung, die den Interpreter-Pfad eines anderen Hosts wiederverwendet ([ansible/ansible#87288](https://github.com/ansible/ansible/issues/87288)): `documented`, jeder Host bekommt seine eigene Erkennung;
- die Warnung der Erkennung, die den Inventory-Host statt des delegierten Hosts nennt ([ansible/ansible#87298](https://github.com/ansible/ansible/issues/87298)): `documented`;
- Erkennung, die für delegierte Hosts nicht zwischengespeichert wird ([ansible/ansible#86517](https://github.com/ansible/ansible/issues/86517)): `documented`, da sich nur die Zeit unterscheidet;
- `dnf`, das fehlschlägt, wenn ein dnf-Plugin auf stdout schreibt ([ansible/ansible#87366](https://github.com/ansible/ansible/issues/87366)): `release`. Das Modul läuft aus dem ansible-core des Controllers, und Volant patcht keine Module. Eine Änderung an der Ausgabe eines erfolgreichen `dnf` wurde weder im Changelog noch im Issue-Tracker gefunden.

### Der Lock

`volant.lock` legt die Engine fest, die ein Projekt ausführt. Ob er das Profil festhält und was ein Lauf tut, wenn Lock und `core_version` nicht übereinstimmen, wird in #653 entschieden.

## Konsequenzen

- Ein Projekt, das nichts einstellt, behält das Verhalten von `2.19`.
- Ein Projekt, das zu `latest` wechselt, behält seine entfernten Features mit einer Warnung, außer den sicherheitsrelevanten.
- Jeder Unterschied kostet einen Schalter, einen Registry-Eintrag und eine Aufzeichnung, bevor er in ein Profil kommt.
- Die Umsetzung folgt in diesen Issues, nach Größe:
  - die Profileinstellung, `VOLANT_CORE_VERSION`, `ansible_version` pro Profil und die Prüfung des ansible-core auf dem Controller (mittel);
  - Aufzeichnen pro Profil: das Versionsargument von `just golden`, `expected.<version>.json` und die Golden-Tests pro Profil (mittel);
  - die Registry, ihre Warnungen und `allow_insecure_legacy` (mittel);
  - die Schalter des Profils `2.22` nach dem Anhang unten (groß, nach Bereichen aufgeteilt, sobald die Einstellung steht);
  - `volant upgrade-plan` (mittel).

## Anhang: Korpus-Diff, 2026-10-25

ansible-core 2.20.10, 2.21.5 und 2.22.0rc1 gegenüber 2.19.12. Die ersten drei liefen unter Python 3.12, 2.22.0rc1 unter Python 3.13. Weggelassen: die Reihenfolge der Hosts unter `order: shuffle` und unter `throttle`, die von Lauf zu Lauf schwankt; Pfade von Job-Dateien und Prozess-IDs; `ansible_version` selbst; und die Template-Unterschiede, die von Python statt von ansible-core kamen.

| Feature | 2.19.12 | 2.20 | 2.21 | 2.22 | Art |
|---|---|---|---|---|---|
| Test `regex` mit unbekanntem `match_type` | `'str' object is not callable` | `Invalid match_type specified` | wie 2.20 | wie 2.20 | Template |
| `password_hash` mit vorgegebenem Salt | verwendet das Salt (`$1$saltsalt$…`) | wie 2.19 | ein anderes Salt (`$1$n34PoBLM$…`) | wie 2.21 | Template |
| Fehler in einem `when` | `Error while evaluating conditional: …` | wie 2.19 | `A 'when' expression failed: …` | wie 2.21 | Template |
| `msg` eines fehlgeschlagenen Befehls | `non-zero return code` | wie 2.19 | `The command exited with a non-zero return code.` | wie 2.21 | Modul |
| Fehler von `include_vars` | Pfad ohne Anführungszeichen | Pfad in Anführungszeichen | wie 2.20 | wie 2.20 | Modul |
| Check-Mode-Ergebnis von `copy` | kein Schlüssel `file` | Schlüssel `file` | wie 2.20 | wie 2.20 | Modul |
| `command`, dessen `chdir` fehlschlägt | `rc: null` | wie 2.19 | kein Schlüssel `rc` | wie 2.21 | Modul |
| `cmd` eines nicht gefundenen Befehls | ein String | wie 2.19 | wie 2.19 | eine Liste | Modul |
| `ansible_parsed` in Modulergebnissen | fehlt | fehlt | fehlt | `true` | Modul |
| `skipped: false` in Schleifenergebnissen | vorhanden | vorhanden | fehlt | fehlt | Szenario |
| `changed_when_result` | fehlt | fehlt | vorhanden | vorhanden | Szenario |
| Meldung von `failed_when` | `Action failed: Unknown error.` oder der Name der Aktion | wie 2.19 | `A 'failed_when' expression evaluated to 'True'.` | wie 2.21 | Szenario |
| Wiederholungszeile von `until` und `attempts` | `(1 retries left)`, `attempts: 1` | wie 2.19 | `(0 retries left)`, `attempts: 2` | wie 2.21 | Szenario |
| Schleifenelemente, die einen in derselben Schleife gesetzten Fact lesen | `a-1`, `b-2`, `c-3` | wie 2.19 | `a-0`, `b-1`, `c-2` | wie 2.21 | Szenario |
| `any_errors_fatal: "{{ false }}"` nach einem Fehler | der andere Host stoppt | wie 2.19 | der andere Host macht weiter | wie 2.21 | Szenario |
| Fehlende Datei in `include_tasks` | Schlüssel `include` und `reason` | wie 2.19 | `msg` und eine `[ERROR]`-Zeile | wie 2.21 | Szenario |
| Fehlende Quelle von `copy` | einzeilige Meldung | wie 2.19 | `<<< caused by >>>` und die durchsuchten Pfade | wie 2.21 | Szenario |
| Rollendatei außerhalb der Rolle | Exit-Code 4 | Exit-Code 4 | Exit-Code 1 | Exit-Code 1 | Szenario |
| `debug` eines undefinierten `msg` | `{"msg": null}` | wie 2.19 | `{}` | wie 2.21 | Szenario |
| `--start-at-task` mit einer Argument-Spec der Rolle | Prüfung übersprungen | Prüfung läuft | wie 2.20 | wie 2.20 | Szenario |
| `ansible --help` | nur `-K` | `-K \| --become-password-file` | wie 2.20 | wie 2.20 | Szenario |
| Geheime Werte in der Ausgabe | angezeigt | angezeigt | angezeigt | `$REDACTED$`, selbst in einem Pfad | Szenario |
| Mit Vault verschlüsselter Wert in `ansible-inventory --graph` | angezeigt | angezeigt | angezeigt | `$REDACTED$` | Inventory |

Unter 2.22 hat das Maskieren auch Teile der Ausgabe verdeckt, die keine Geheimnisse sind: Das become-Passwort `wrong` hat das Verzeichnis `become-su-local-wrong-password` in `become-su-local-$REDACTED$-password` verwandelt.
