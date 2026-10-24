---
sourceHash: aaa01d3d7aed
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

## Dateien verschlüsseln und anzeigen

`volant vault encrypt`, `decrypt` und `view` arbeiten mit denselben Dateien wie `ansible-vault`: Eine vom einen verschlüsselte Datei liest das andere, mit der Standard-ID wie mit einem Label.

- `encrypt` ersetzt jede Datei durch ihren Vault-Text und `decrypt` durch ihren Klartext. Beide lassen das Ergebnis im Modus `0600` und geben nichts aus; in einem Terminal schreiben sie `Encryption successful` oder `Decryption successful` auf die Standardfehlerausgabe. Eine Datei ohne abschließenden Zeilenumbruch kommt ohne einen zurück.
- Der Header lautet `$ANSIBLE_VAULT;1.1;AES256` für die Standard-ID und `$ANSIBLE_VAULT;1.2;AES256;LABEL` für eine andere. `--vault-id LABEL@SOURCE` setzt das Label. Bei mehr als einer `--vault-id` wählt `--encrypt-vault-id` (oder `vault_encrypt_identity`, `ANSIBLE_VAULT_ENCRYPT_IDENTITY`) die verschlüsselnde aus; ohne sie endet der Befehl mit Exit-Code 5.
- `--output FILE` schreibt dorthin und lässt die Quelle unverändert, `--output -` schreibt auf die Standardausgabe, und ohne Dateiname oder mit `-` wird die Standardeingabe gelesen.
- `view` gibt den Klartext jeder Datei aus. In einem Terminal läuft er über `ANSIBLE_PAGER`, `PAGER` oder `less`; die `ansible.cfg` hat beim Pager nichts zu sagen.
- Ohne jede Passwortquelle fragt der Befehl nach dem Passwort. `encrypt` fragt zweimal, als `New Vault password: ` und `Confirm New Vault password: `.
- `-v` gibt die verwendete Konfigurationsdatei aus und `-vv` die Version. Die zusätzlichen Zeilen von `-vvvv` werden nicht ausgegeben.

## Bekannte Lücken

- Die Argumente `salt` und `wrap_object` des Filters `vault` werden als noch nicht unterstützt abgelehnt.
- Ein `!vault`-Wert in den eigenen `vars:` eines Plays gibt keine Zeile `Origin:` aus, wenn kein Geheimnis ihn öffnet.
- Ein `set_fact` mit einem bloßen `{{ x }}`, das einen verschlüsselten Wert enthält, verliert das Vault-Tag, sodass `is vault_encrypted` beim Fact false liefert, wo ansible-core true liefert.

Die [Seite zur Kommandozeile](/de/docs/reference/cli/#vault) enthält die Optionen.
