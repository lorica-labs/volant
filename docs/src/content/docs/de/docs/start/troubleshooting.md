---
sourceHash: 85339820dcff
title: Wenn ein Lauf fehlschlägt
description: Finden Sie den fehlgeschlagenen Task und die Ursache, prüfen Sie dasselbe Playbook unter ansible-playbook und melden Sie, was Sie gefunden haben.
---

Beginnen Sie beim Exit-Code. Er sagt Ihnen, ob der Lauf angehalten hat, bevor er einen Host berührte, oder auf einem Host fehlgeschlagen ist.

| Code | Was geschehen ist | Wo Sie nachsehen |
|---|---|---|
| 4, ohne Recap | Das Playbook ist vor der ersten Verbindung abgebrochen. Es lässt sich nicht parsen, oder es verwendet etwas, das dieses Release noch nicht unterstützt. | Die Meldung über dem Code. Siehe [Abbruch vor dem Lauf](#abbruch-vor-dem-lauf). |
| 2, 4 oder 6, mit Recap | Ein Host ist fehlgeschlagen (2), ein Host war nicht erreichbar (4), oder beides (6). | Die Zeile `fatal:` des Tasks, dann [Fehler während des Laufs](#fehler-während-des-laufs). |
| 1 | Eine Rolle, ein Playbook oder ein Host-Muster wurde nicht gefunden. | Die Datei oder der Name, den die Meldung zitiert. |

[Exit-Codes](/de/docs/reference/exit-codes/) listet jeden Code auf.

## Abbruch vor dem Lauf

Das meiste, was dieses Release nicht ausführen kann, wird benannt, bevor Volant sich mit irgendetwas verbindet. Die Meldung sagt, was abgelehnt wurde und wo:

| Meldung | Was zu tun ist |
|---|---|
| `task 'NAME': keyword 'KEYWORD' is not supported yet on 'MODULE'` | Prüfen Sie das Keyword unter [Keywords](/de/docs/reference/keywords/). Entfernen Sie es, oder lassen Sie dieses Play vorerst unter `ansible-playbook`. |
| `task 'NAME': argument 'ARGUMENT' is not supported yet on 'MODULE'` | Prüfen Sie das Modul unter [Module](/de/docs/reference/modules/). Für die meisten abgelehnten Argumente gibt es dort einen Workaround. |
| `couldn't resolve module/action 'NAME'` | Prüfen Sie die Schreibweise. Stammt das Modul aus einer Collection, installieren Sie sie mit `volant runtime install --collections NAMESPACE.COLLECTION`. |
| `strategy 'NAME' is not supported yet` | Verwenden Sie `linear`, `free`, `host_pinned` oder `debug`. |
| `become_method 'NAME' is not supported yet` | Verwenden Sie `sudo` oder `su`. Dieser Fall endet mit 2. |
| `the role 'NAME' was not found in ...` | Die Meldung listet die durchsuchten Verzeichnisse auf. Fügen Sie das Verzeichnis der Rolle zu `roles_path` hinzu. Dieser Fall endet mit 1. |

[Läuft mein Playbook?](/de/docs/start/compatibility/#was-heute-abgelehnt-wird) listet diese Ablehnungen auf und zeigt, wie Sie ein ganzes Projekt prüfen, ohne es auszuführen. [Die Vorabprüfung](/de/docs/internals/preflight/) erklärt, wie die Prüfung funktioniert.

## Fehler während des Laufs

1. Führen Sie den Lauf erneut nur auf dem fehlschlagenden Host aus, mit mehr Ausgabe. `-vvv` gibt die Argumente aus, die jedes Modul erhalten hat:

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 -vvv
   ```

2. Überspringen Sie die Tasks, die bereits durchlaufen. `--start-at-task` beginnt bei dem Task, den Sie nennen, und `--step` fragt vor jedem Task nach:

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 --start-at-task "Install nginx"
   ```

3. Halten Sie am Fehler an und sehen Sie sich um. Mit `debugger: on_failed` am Play oder am Task öffnet Volant den Task-Debugger von Ansible, wenn ein Task fehlschlägt. Sie können Variablen ausgeben, die Argumente ändern und den Task erneut ausführen.

## Mit ansible-playbook vergleichen

Volant liest dieselben Dateien, also läuft derselbe Befehl unter Ansible:

```sh
ansible-playbook -i inventory.ini site.yml --limit web1
```

Schlägt Ansible genauso fehl, liegt das Problem im Playbook oder auf dem Host. Schlägt nur Volant fehl, oder liefern beide unterschiedliche Ergebnisse, ist das ein Bug in Volant. Sie können das Play unter `ansible-playbook` lassen, bis er behoben ist.

## Zurück zu ansible-playbook

Dieselben Dateien laufen unverändert unter `ansible-playbook`. Volant fügt Ihren Playbooks, Rollen, Inventories und Ihrer `ansible.cfg` nichts hinzu, es gibt also nichts zurückzukonvertieren.

Auf den Hosts bleibt nur der zwischengespeicherte Agent: ein Verzeichnis namens `volant-agent-<version>-<uid>` in `~/.ansible/tmp` und daneben ein Verzeichnis `volant-blobs-<version>-<uid>`, wenn ein Lauf Python-Module verwendet hat. Um sie zu entfernen, löschen Sie sie auf jedem Host als das Konto, das der Lauf verwendet hat:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-*'
```

Auf einem Host, auf dem Tasks mit `become` liefen, liegt eine zweite Kopie im Home-Verzeichnis des Kontos, auf das gewechselt wurde. Führen Sie denselben Befehl als dieses Konto aus. Wie Sie Volant auch vom Controller entfernen, steht unter [Deinstallieren](/de/docs/start/installation/#deinstallieren). [Was Volant auf Ihren Rechnern ablegt](/de/docs/#was-volant-auf-ihren-rechnern-ablegt) listet alles auf.

## Melden

- Eine Abweichung von `ansible-playbook` gehört in einen Bericht zu einer [Lücke in der Ansible-Kompatibilität](https://github.com/lorica-labs/volant/issues/new?template=compat.yml).
- Alles andere, was nicht wie dokumentiert funktioniert, gehört in einen [Bug-Report](https://github.com/lorica-labs/volant/issues/new?template=bug.yml).
- Ein Sicherheitsproblem melden Sie über die [Sicherheitsrichtlinie](https://github.com/lorica-labs/volant/blob/main/SECURITY.md), nie über ein öffentliches Issue.

Fügen Sie die Ausgabe von `volant --version` bei, das kleinste Playbook samt Inventory, das noch fehlschlägt, und die Ausgabe des fehlschlagenden Laufs mit `-vvv`. Entfernen Sie Passwörter, Schlüssel, Vault-Inhalte und Hostnamen, bevor Sie sie posten. Ein öffentliches Issue bleibt öffentlich.
