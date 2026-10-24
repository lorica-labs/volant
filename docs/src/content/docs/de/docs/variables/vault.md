---
sourceHash: 06ef07eefbf0
title: Vault
description: Wie Volant Ansible-Vault-Daten liest, woher es die Passwörter bekommt und warum sie den Controller-Prozess nie verlassen.
---

Volant liest und schreibt die Vault-Formate 1.1 und 1.2 von ansible-core 2.19.12. Ergebnisse, Fehlermeldungen und Exit-Codes stimmen mit ansible-core überein.

## Woher die Passwörter kommen

| Quelle | Hinweise |
|---|---|
| `-J`, `--ask-vault-pass` | Wird im Terminal abgefragt, ohne Echo. |
| `--vault-password-file`, `--vault-pass-file` | Eine Datei oder eine ausführbare Datei, die das Passwort ausgibt. Wiederholbar. |
| `--vault-id LABEL@SOURCE` | Die Quelle ist eine Datei, eine ausführbare Datei oder `prompt`. Wiederholbar. |
| `[defaults] vault_password_file`, `vault_identity_list` | Dasselbe, aus der `ansible.cfg`. |
| `ANSIBLE_VAULT_PASSWORD_FILE`, `ANSIBLE_VAULT_IDENTITY_LIST` | Dasselbe, aus der Umgebung. |

Jedes Passwort wird bei jedem verschlüsselten Wert versucht, unabhängig von seinem Label.

## Was entschlüsselt wird

- Inline-Werte mit `!vault` in einem Playbook oder einer Vars-Datei. Sie werden als Chiffretext geladen und entschlüsselt, wenn ein Template sie liest. Ein entschlüsselter Wert behält das Vertrauen, das ansible-core ihm gibt.
- Ganze Vault-Dateien: `vars_files`, `include_vars`, Dateien in `group_vars` und `host_vars`. Eine verschlüsselte Datei, die sich nicht lesen lässt, schlägt mit der Meldung von ansible-core fehl.
- Die Filter `vault` und `unvault`, die Tests `vault_encrypted` und `vaulted_file` und das Lookup `unvault`.
- Ein verschlüsseltes `src` von `copy`, `template` und `script` sowie ein verschlüsseltes Archiv bei `unarchive`. Der Lauf entschlüsselt es in ein privates temporäres Verzeichnis, bevor das Plugin es liest. `decrypt: false` bei `copy` oder `unarchive` behält den Chiffretext.

Ein `!vault`-Wert, den kein Geheimnis öffnet, lässt den Task fehlschlagen, der ihn liest, mit den drei Blöcken, die ansible-core ausgibt. `debug: var=x` endet mit 2, mit der Ursache in der Zeile, in der der Wert geschrieben steht. `is vault_encrypted` antwortet für die Variablen des aktuellen Hosts.

## Geheimnisse bleiben auf dem Controller

Ein Vault-Passwort liegt im Speicher des Controller-Prozesses. Es wird nie in eine Kommandozeile, in die Umgebung eines Kindprozesses, in ein Log, in ein Ergebnis oder in die Agent-Verbindung geschrieben. Die Passwörter für Verbindung und Rechteausweitung folgen derselben Regel, und [Verbindungen](/de/docs/hosts/connections/#verbindungstypen) sagt, wie das SSH-Passwort `ssh` erreicht.

Filter, Tests und Lookups, die in Python laufen, sehen das Vault-Passwort nie. Die Vault-Varianten laufen aus diesem Grund im Controller-Prozess. Siehe [Filter, Tests und Lookups, die Python brauchen](/de/docs/variables/templating/#filter-tests-und-lookups-die-python-brauchen).

Ein Wert, der von einem verwalteten Host zurückkam, wird nie mit den Geheimnissen des Laufs entschlüsselt und nie erneut als Template gerendert. Siehe [Werte von Hosts werden nie als Template gerendert](/de/docs/variables/templating/#werte-von-hosts-werden-nie-als-template-gerendert).

## Bekannte Lücken

- Die Argumente `salt` und `wrap_object` des Filters `vault` werden als noch nicht unterstützt abgelehnt.
- Ein `!vault`-Wert in den eigenen `vars:` eines Plays gibt keine Zeile `Origin:` aus, wenn kein Geheimnis ihn öffnet.
- Ein `set_fact` mit einem bloßen `{{ x }}`, das einen verschlüsselten Wert enthält, verliert das Vault-Tag, sodass `is vault_encrypted` beim Fact false liefert, wo ansible-core true liefert.

Die [Seite zur Kommandozeile](/de/docs/reference/cli/#vault) enthält die Optionen.
