---
sourceHash: 74edd923ede7
title: Check-Modus, Diff und Auflistungen
description: Was --check und --diff tun, welche Module sie beantworten, wie Tags Tasks auswählen und welche Befehle einen Lauf beschreiben, ohne ihn zu starten.
---

Diese Seite beantwortet, wie Sie einen Lauf in der Vorschau ansehen, bevor er etwas ändert: Probeläufe mit `--check`, Dateiänderungen mit `--diff`, die Auswahl von Tasks per Tag und Auflistungen, die sich mit nichts verbinden.

`--check` meldet, was jeder Task ändern würde, ohne es zu ändern. `--diff` gibt die Änderungen aus, die ein Modul an Dateien vornimmt. Beide funktionieren wie bei `ansible-playbook` 2.19.12, und Recap und Exit-Code stimmen überein.

Die Task-Keywords schlagen die Optionen. `check_mode: false` führt den Task unter `--check` wirklich aus. `check_mode: true` macht einen einzelnen Task in einem normalen Play zum Probelauf. `diff: false` hält die Änderungen eines Tasks unter `--diff` vom Bildschirm fern.

`ansible_check_mode` und `ansible_diff_mode` geben nur `--check` und `--diff` von der Kommandozeile wieder. Ein eigenes Keyword `check_mode` oder `diff` an einem Play oder Task ändert sie nicht.

## Was wo läuft

Ein Task im Check-Modus ändert nie einen Host, welcher Pfad ihn auch ausführt.

Volant bringt für einige Module eine native Version mit, in Rust geschrieben und im Agent ausgeführt, dem kleinen Programm, das es auf jeden Host hochlädt. Kann die native Version einen Task nicht beantworten, gibt sie ihn zurück: Stattdessen läuft der eigene Python-Code des Moduls auf dem Host.

| Task | Was geschieht |
|---|---|
| `command`, `shell`, `raw` | Werden wie in ansible-core übersprungen. Ein `command` oder `shell` mit `creates` oder `removes`, in der Zeile oder in `args:`, wird geprüft: `ok`, wenn die Bedingung ihn aufhält, `changed`, wenn er liefe. |
| `stat`, `setup`, `package_facts`, `service_facts` | Werden vom Agent beantwortet. Sie lesen nur, daher ändern Check und Diff nichts. |
| `file`, `lineinfile`, `systemd`, `user`, `group`, `apt` | Werden an den Python-Code des Moduls auf dem Host zurückgegeben, der weiß, was er geändert hätte und wie der Diff zu formulieren ist. |
| `copy`, `template`, `fetch`, `unarchive` und andere Action-Plugins | Werden vom eigenen Plugin von ansible-core über den Python-Helfer des Controllers ausgeführt (ein Prozess, der ansible-core auf dem Controller ausführt), der das Check-Ergebnis und den Diff selbst berechnet. |
| Controller-Module (`debug`, `set_fact`, `assert`, ...) | Laufen wie gewohnt. |
| `package`, `service`, `dnf` | Wählen das Modul für den Host, dann läuft dieses Modul im Check-Modus. |

Das Profil (`--profile`) nennt den Grund jeder Rückgabe: „the task runs in check mode“ oder „the task shows a diff“.

## Ein Host ohne Python

Das Zurückgeben braucht Python auf dem Host. Ohne Python schlägt jedes native Modul, das den Host ändert, unter `--check` oder `--diff` fehl: `apt`, `user`, `group`, `file`, `lineinfile`, `systemd`, `systemd_service` und das Modul `copy`, das das Plugin `copy` ausführt. Installieren Sie Python auf dem Host, oder führen Sie das Play ohne diese Optionen aus. Tasks, die nur lesen (`stat`, `setup`, `package_facts` und `service_facts`), sowie `command`, `shell` und `raw` funktionieren weiterhin.

## Diff-Ausgabe

Ein Diff wird nur für einen Task ausgegeben, der etwas geändert hat. `copy` und `template` geben nichts aus, wenn `diff` aus ist. Der Helfer-Prozess startet nur, wenn ein Diff ausgegeben wird.

## Bekannte Lücken

- `--step` fragt nicht vor einem `meta`- oder `flush_handlers`-Schritt nach. Das wurde nicht mit ansible-core verglichen.
- `-vv` gibt die Benachrichtigungszeilen der Handler nicht aus.
- `-vvvv` listet die Optionen auf, die ansible-core auflistet, in dessen Reihenfolge, und die Zeilen zur Einrichtung der Plugins. Seine ssh-Zeilen zeigen die Befehle, die diese Engine über `ssh` ausführt (die Sonde, den Upload und den Link zum Agent), nicht den `sftp`-Upload von ansible-core, und eine lokale Verbindung oder eine Moduldatei gibt keine Zeile aus.
- Ein Task mit einem fehlenden Schlüssel in einem übersprungenen Ergebnis liefert eine andere Meldung als ansible-core.

Die [Paritätsseite](/de/docs/reference/parity/) listet jede Option und jedes Keyword mit seinem Status auf.

## Tags

`tags` können an einem Play, einem Rolleneintrag, einem Block oder einem Task stehen. Der Compiler faltet äußere Tags in jeden Task darunter. `--tags` und `--skip-tags` entscheiden dann, welche Tasks überhaupt einkompiliert werden, sodass Tags sowohl bestimmen, was läuft, als auch, was die Auflistungen zeigen.

### Die Tag-Regeln

Volant wendet die Regeln von Ansible genau an:

| Tag am Task | Verhalten |
|---|---|
| `always` | Wird von jedem `--tags` ausgewählt. Übersteht `--skip-tags all`, es sei denn, `always` wird selbst übersprungen. |
| `never` | Wird von `--tags all` und `--tags tagged` ausgelassen. Kommt nur zurück, wenn eine Auswahl ihn nennt. |
| kein Tag | Wird als Tag `untagged` behandelt, weshalb `--tags untagged` ihn auswählt. |

`[tags] run` und `[tags] skip` in `ansible.cfg` sowie `ANSIBLE_RUN_TAGS` und `ANSIBLE_SKIP_TAGS` sind Standardwerte. Die Kommandozeile ergänzt sie, statt sie zu ersetzen.

Tags an einem `include_tasks` oder `include_role` enden an der Anweisung: `--tags inc` führt das Include aus, nicht das, was es einbringt. Die Tags des Plays, des Rolleneintrags und der Blöcke um die Anweisung erreichen dagegen die eingebundenen Tasks, sodass `--tags nginx` bei einer Rolle mit dem Tag `nginx` die Dateien ausführt, die diese Rolle einbindet. Ansible verhält sich genauso.

### `ansible_run_tags` und `ansible_skip_tags`

Ein Task kann die Tag-Auswahl seines eigenen Laufs als zwei Variablen zurücklesen: `ansible_run_tags` enthält `--tags` (`["all"]`, wenn es nicht angegeben wurde) und `ansible_skip_tags` enthält `--skip-tags` (`[]`, wenn es nicht angegeben wurde), jeweils nachdem `[tags] run` / `[tags] skip` der `ansible.cfg` und die beiden Umgebungsvariablen `ANSIBLE_RUN_TAGS` / `ANSIBLE_SKIP_TAGS` eingerechnet sind.

```yaml
- name: Only fetch the kubeconfig once it has been asked for
  fetch:
    src: /etc/rancher/k3s/k3s.yaml
    dest: "{{ kubeconfig }}"
  when: "'kubeconfig' in ansible_run_tags"
```

## Vor dem Lauf ansehen

Vier Optionen lesen ein Playbook und geben aus, was ein Lauf tun würde, ohne sich mit irgendetwas zu verbinden:

```sh
volant playbook -i inventory.ini site.yml --list-hosts
volant playbook -i inventory.ini site.yml --list-tasks
volant playbook -i inventory.ini site.yml --list-tags
volant playbook -i inventory.ini site.yml --syntax-check
```

Ihre Ausgabe stimmt bis auf den Leerraum mit der von Ansible überein. Die Testsuite vergleicht sie Byte für Byte mit ansible-core über einen Korpus von Playbooks.

Auflistungen durchlaufen nur den Rumpf eines Blocks, ein Task in `rescue:` oder `always:` erscheint also nicht, genau wie in Ansible.

## Unterschiede zu Ansible

- Ein Play mit mehreren Tags gibt sie sortiert aus, und `--list-hosts` gibt die Hosts eines Plays in Inventory-Reihenfolge aus. Ansible gibt an beiden Stellen ein Python-Set aus, dessen Reihenfolge sich von Lauf zu Lauf ändert.
- Eine Auflistung löst keine Module auf. Ein Playbook, dessen Module dieses Release nicht ausführen kann, wird trotzdem aufgelistet und besteht trotzdem `--syntax-check`, sodass Sie eine Rolle lesen können, bevor das Release erscheint, das sie ausführt.
- Ein Task, dessen `ignore_errors` oder `become` weder true noch false ist, lässt eine Auflistung mit Code 4 enden. Ansible listet ihn auf und endet mit 0. Der Wert ist ein fehlerhaftes Playbook, keine nicht umgesetzte Funktion.
