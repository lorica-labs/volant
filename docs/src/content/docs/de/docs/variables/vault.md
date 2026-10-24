---
sourceHash: 06260172771b
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
- `view` gibt den Klartext jeder Datei aus. In einem Terminal läuft er über den Pager: `PAGER`, sonst `ANSIBLE_PAGER`, sonst `[defaults] pager`, sonst `less`.
- Ohne jede Passwortquelle fragt der Befehl nach dem Passwort. `encrypt` fragt zweimal, als `New Vault password: ` und `Confirm New Vault password: `.
- `-v` gibt die verwendete Konfigurationsdatei aus und `-vv` die Version. Die zusätzlichen Zeilen von `-vvvv` werden nicht ausgegeben.

## Dateien anlegen, bearbeiten und neu verschlüsseln

`volant vault create`, `edit` und `rekey` arbeiten mit denselben Dateien wie `ansible-vault`.

- `create` nimmt einen Dateinamen. Es verweigert mit Exit-Code 5, wenn die Standardausgabe kein Terminal ist und `--skip-tty-check` fehlt, und mit Exit-Code 1, wenn die Datei existiert. Ein fehlendes Verzeichnis wird mit einer Warnung angelegt. Der Editor darf die Datei leer lassen: Das Ergebnis ist eine gültige Vault-Datei mit leerem Text.
- `edit` entschlüsselt jede Datei, öffnet den Editor und verschlüsselt das Ergebnis mit dem Geheimnis, das die Datei geöffnet hat, und mit dem Label, das sie hatte. Hat sich der Text nicht geändert, wird die Datei nicht neu geschrieben. Eine geänderte Datei behält ihren Modus.
- Der Editor ist `EDITOR`, sonst `ANSIBLE_EDITOR`, sonst `[defaults] editor`, sonst `vi`, so in Wörter zerlegt, wie es eine Shell täte, mit angehängtem Dateinamen. Sein Exit-Status wird nicht ausgewertet, wie bei ansible-vault.
- Der Editor arbeitet an einer Kopie mit Modus `0600` in einem privaten Verzeichnis unter `ANSIBLE_LOCAL_TEMP` (`[defaults] local_tmp`, sonst `~/.ansible/tmp`). Die Kopie wird bei jedem Ausstieg überschrieben und entfernt, auch bei einem fehlschlagenden Editor und bei SIGTERM oder SIGHUP. Strg-C geht an den Editor.
- `rekey` entschlüsselt jede Datei mit den alten Geheimnissen und schreibt sie mit dem neuen wieder, unter Beibehaltung des Modus. Das neue Geheimnis kommt aus `--new-vault-password-file`, aus `--new-vault-id LABEL@SOURCE` oder aus den Abfragen `New Vault password: ` und `Confirm New Vault password: `. Es gibt `Rekey successful` auf der Standardfehlerausgabe aus, was auch immer die Standardausgabe ist.

## Strings verschlüsseln

`volant vault encrypt_string` macht aus einem Wert einen `!vault |`-Block für eine Variablendatei. Jede Zeile des Chiffretexts ist um zehn Leerzeichen eingerückt, und jeder Block endet mit einem Zeilenumbruch:

```yaml
mykey: !vault |
          $ANSIBLE_VAULT;1.1;AES256
          66313561363937653234366564623639...
```

- `-n NAME` benennt den String an derselben Stelle und `--stdin-name NAME` benennt die Standardeingabe. Ohne Namen beginnt der Block mit `!vault |`. `--vault-id LABEL@SOURCE` ergibt einen `1.2`-Header mit dem Label.
- Ohne String oder mit `-` wird der Text bis zum Ende von der Standardeingabe gelesen. Leere Eingabe endet mit Exit-Code 5.
- Bei mehr als einem String enthält die Standardfehlerausgabe pro String eine Kommentarzeile und einen Hinweis, wenn es weniger Namen als Strings gibt. Die Standardausgabe enthält nur die Blöcke.
- `-p` fragt nach einem Variablennamen, dann nach dem String, der nicht angezeigt wird, außer `--show-input` ist angegeben. Es lässt sich nicht mit dem Lesen der Standardeingabe kombinieren.
- In einem Terminal geht `Encryption successful` vor den Blöcken auf die Standardfehlerausgabe.
- `--output FILE` schreibt die Blöcke in die Datei, im Modus `0600`.

## Bekannte Lücken

- Die Argumente `salt` und `wrap_object` des Filters `vault` werden als noch nicht unterstützt abgelehnt.
- Ein `!vault`-Wert in den eigenen `vars:` eines Plays gibt keine Zeile `Origin:` aus, wenn kein Geheimnis ihn öffnet.
- Ein `set_fact` mit einem bloßen `{{ x }}`, das einen verschlüsselten Wert enthält, verliert das Vault-Tag, sodass `is vault_encrypted` beim Fact false liefert, wo ansible-core true liefert.

Die [Seite zur Kommandozeile](/de/docs/reference/cli/#vault) enthält die Optionen.
