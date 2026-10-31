---
sourceHash: f054303cc0e6
title: Delegation, Meta-Tasks und Umgebungsvariablen
description: Einen Task einmal pro Batch oder auf einem anderen Host ausführen, was jede meta-Action tut und wie environment und no_log einen Task prägen.
---

Diese Seite beantwortet, wie Sie einen Task an einem anderen Ort ausführen als dort, wo das Play gerade arbeitet, wie Sie den Lauf selbst mit `meta` steuern und wie Sie Umgebungsvariablen für einen Task setzen oder Ausgaben verbergen.

## run_once

`run_once` führt einen Task auf einem Host des Batches aus und gibt jedem anderen Host, was er erzeugt hat, einschließlich allem, was er registriert hat.

```yaml
- name: Run database migrations once
  command: /opt/app/migrate.sh
  run_once: true
```

Die Hosts des Batches treffen sich vor einem solchen Task, bevor einer von ihnen ihn ausführt. Die Wahl muss für alle dieselbe sein, und die anderen Hosts müssen auf den warten, der läuft.

`run_once` kann ein Template sein. Jeder Host des Batches rendert es nacheinander, und der erste Host, für den es wahr ist, führt den Task aus. Ein Template, das im Batch unterschiedlich antwortet, beendet den Lauf mit Exit 4.

Unter `strategy: free` und `host_pinned` wird `run_once` mit einer Warnung ignoriert, und der Task läuft für jeden Host. Ein `run_once`-Task, der dort fehlschlägt, lässt nur seinen eigenen Host fehlschlagen. Siehe [Andere Strategien](/de/docs/playbooks/how-a-play-runs/#andere-strategien).

## delegate_to

`delegate_to` führt einen Task an einem anderen Ort aus als auf dem Host, für den er läuft.

```yaml
- name: Take the host out of the load balancer
  command: /usr/local/bin/lb-drain {{ inventory_hostname }}
  delegate_to: lb1
```

Der Task läuft über die eigene Verbindung des Delegierten, mit dessen Verbindungseinstellungen. Das Ergebnis gehört weiterhin dem Host, an dem das Play arbeitet: Der Recap zählt diesen Host, und die Ergebniszeile lautet `[web1 -> lb1]`.

- Ein Task, der durch `when` übersprungen wurde, hat den Delegierten nie erreicht, seine Zeile `skipping:` hat daher keinen Pfeil.
- Die eigene Verbindung des delegierenden Hosts wird für einen delegierten Task nicht geöffnet, sodass ein Task, der von einem nicht erreichbaren Host wegdelegiert wurde, trotzdem läuft.
- Der Delegierte wird im gesamten Inventory gesucht, nicht nur unter den Hosts des Plays. Ein Play über zwei Hosts kann an einen dritten delegieren, der nicht im Recap erscheint.
- Ein Name, den das Inventory nicht kennt, wird als SSH-Ziel dieses Namens behandelt und als nicht erreichbar gemeldet, wenn das fehlschlägt. Ein nicht erreichbarer Delegierter nimmt den delegierenden Host aus dem Lauf.

`delegate_facts: true` speichert, was der Task erzeugt hat, auf dem Delegierten statt auf den Hosts, die ihn ausgeführt haben.

Ein Modul, das auf dem Controller läuft, etwa `set_fact` oder `debug`, läuft immer auf dem Controller, was `delegate_to` auch sagt. Der Delegierte bestimmt dann nur den Namen in der Ergebniszeile und, mit `delegate_facts`, wessen Facts geschrieben werden.

`action:` und `local_action:` nehmen einen String (`command echo hi`), ein Mapping mit `module:`, das Ansible als veraltet kennzeichnet, oder einen bloßen Modulnamen mit den Argumenten unter `args:`. Das Modul darf ein Template sein, etwa `action: "{{ module_to_run }} msg=hi"`: Es wird für jeden Host gerendert, wenn der Task läuft, und das Banner zeigt es so, wie es geschrieben ist. Die Module werden vor der ersten Verbindung gebaut, deshalb baut ein Lauf mit einem solchen Template jedes eingebaute Python-Modul. Ein Template, das ein Modul einer Collection oder eines `library/`-Verzeichnisses nennt, das kein Task direkt nennt, lässt den Task fehlschlagen. Ein Wort wie `module=command` nach `action:` nennt kein Modul. Der Task schlägt fehl, wenn ein Host ihn erreicht, mit Exit 2, und ein `rescue` oder `ignore_errors` übernimmt den Fehler, wie in Ansible.

`delegate_to` und `delegate_facts` können an einem Block und an einem Task stehen, nicht an einem Play, wo Ansible sie ebenfalls akzeptiert. Die beiden Keywords lassen sich kombinieren: Ein `run_once`-Task mit einem `delegate_to` läuft einmal, auf dem Delegierten.

## Meta-Tasks

`meta` bittet die Engine um etwas, statt ein Modul auszuführen. Jede Action, die Ansible 2.19 hat, läuft hier, und jede gibt ein Banner `TASK [meta]` aus. Eine Action, die Ansible nicht hat, gibt das Banner aus und schlägt dann mit `invalid meta action requested: <action>` und einem Block `Origin` fehl, mit Exit 1, auch wenn ihr `when` falsch ist. Ein Meta-Task fügt dem Recap nichts hinzu: Auch seine Zeile `skipping:` wird nicht gezählt.

| Action | Was sie tut | Wessen `when` zählt |
|---|---|---|
| `flush_handlers` | Führt die Handler aus, von denen der Host weiß, an der Stelle, an der der Task steht. | Das jedes Hosts. |
| `noop` | Nichts. | Wird mit einer Warnung ignoriert. |
| `end_host` | Der Host verlässt das Play. Keine Tasks mehr, keine `always`-Abschnitte, keine Handler. | Das jedes Hosts. |
| `end_role` | Der Host überspringt den Rest der Rolle, in der er sich befindet, einen `always`-Abschnitt der Rolle eingeschlossen. | Das jedes Hosts. |
| `end_batch` | Der Batch endet für jeden Host, und der nächste Batch beginnt. | Das des ersten Hosts. |
| `end_play` | Das Play endet für jeden Host, mit allen Batches, die noch übrig sind. | Das des ersten Hosts. |
| `clear_facts` | Vergisst die Facts, die Module gesammelt haben, auf jedem Host des Batches. Facts aus `set_fact` bleiben. | Das des ersten Hosts. |
| `clear_host_errors` | Verzeiht die Fehler und die nicht erreichbaren Hosts des Batches. Siehe unten. | Das des ersten Hosts. |
| `refresh_inventory` | Liest jede Inventory-Quelle erneut. Siehe unten. | Wird mit einer Warnung ignoriert. |
| `reset_connection` | Schließt die Verbindung des Hosts und die gemeinsame `ssh`-Verbindung mit ihr. Der nächste Task öffnet beide erneut. | Wird mit einer Warnung ignoriert. |

„Der erste Host“ ist der erste Host des Batches, der noch im Play ist, in Inventory-Reihenfolge. Die Actions, die sein `when` lesen, laufen einmal und geben ein Banner aus. Die anderen laufen für jeden Host und geben ein Banner pro Host aus, der noch im Play ist.

`end_role` außerhalb einer Rolle bricht den Lauf vor dem Start ab, mit Exit 4 und der Meldung von ansible-core.

### Hosts, die ausscheiden

Ein Host, der mit `end_host`, `end_batch` oder `end_play` endet, ist nicht fehlgeschlagen, und der Recap zeigt ihn so, wie er stand. In einem `rescue` zählt der aufgefangene Fehler als aufgefangen, und der Host bleibt aus dem Exit-Code heraus. Ein Host, der bereits fehlgeschlagen war, behält seinen Fehler. Spätere Plays laufen wieder auf dem Host.

### Fehler zurücksetzen

`clear_host_errors` nimmt die Fehler des Batches aus dem Exit-Code. Der Recap behält seine Zähler, sodass ein Host `failed=1` zeigen kann, in einem Lauf, der mit 0 endet.

- Ein Host, der fehlgeschlagen ist, kehrt nicht in das Play zurück, in dem er fehlgeschlagen ist. In den Plays danach ist er wieder dabei.
- Ein Host, der nicht erreichbar wurde, kehrt zurück. Er nimmt das Play dort wieder auf, wo er aufgehört hat, und führt die verpassten Schritte aus, nachdem alle anderen Hosts fertig sind. Ist er erneut nicht erreichbar, zählt er erneut.
- Ein Host, der in einem früheren Play nicht erreichbar war, kehrt ebenfalls zurück, ab dem ersten Task des Plays, das zurückgesetzt hat. In den Plays danach ist er wieder nicht erreichbar, und der Exit-Code sagt in beiden Fällen 4.

### Das Inventory neu laden

`refresh_inventory` liest jede Inventory-Quelle erneut, in der Reihenfolge von `-i`. `groups` und `hostvars` ändern sich sofort. Ein Host, den die Quellen hinzugewonnen haben, kommt ab dem nächsten Play dazu, und ein Host, den sie verloren haben, verlässt das Play dort, wo es steht, ohne fehlzuschlagen. Auch `--limit` wird neu aufgelöst, sodass ein Host, den es nennt und den erst die neuen Quellen enthalten, ebenfalls dazukommt. Hosts und Gruppen, die `add_host` und `group_by` angelegt haben, bleiben. Lassen sich die Quellen nicht mehr lesen, ist das Inventory von da an leer, wie in Ansible: Jeder Host verlässt das Play, und die nächsten Plays treffen auf nichts.

Facts und `set_fact`-Werte bleiben, wie sie waren. Siehe [Inventories](/de/docs/hosts/inventories/).

## environment

`environment` setzt Variablen für den Prozess, in dem ein Modul läuft.

```yaml
- hosts: web
  environment:
    http_proxy: http://proxy.internal:3128
  tasks:
    - name: Fetch the release
      command: curl -fsSLO https://example.com/app.tar.gz
      environment:
        CURL_CA_BUNDLE: /etc/ssl/certs/internal.pem
```

Die Schichten werden Schlüssel für Schlüssel zusammengeführt, jede über der vorherigen: zuerst das Play, dann jeder umgebende Block von außen nach innen, dann der Task. Jede Schicht wird mit den Variablen des aktuellen Schleifenelements gerendert.

Werte werden zu dem, was `str()` von Python aus ihnen macht: `42` wird zu `"42"`, `true` zu `"True"` und `null` zu `"None"`. Eine undefinierte Variable in `environment` lässt den Task fehlschlagen.

`environment` darf auch eine Liste von Mappings sein, die der Reihe nach zusammengeführt werden. Ein Eintrag oder ein Wert, der `omit` ist, entfällt.

## no_log

`no_log: true` ersetzt die Ausgabe des Tasks durch die zensierte Zeile, die `ansible-playbook` ausgibt, in jeder Zeile, die er gezeigt hätte, und auf jeder Ausführlichkeitsstufe. Das umfasst die Zeilen `ok:` und `changed:`, `fatal:`, `UNREACHABLE!`, die Zeile jedes Schleifenelements und die Ausgabe von `debug`. Das Label eines Schleifenelements wird zu `(censored due to no_log)`.

Der Block `[ERROR]:`, der über einem Fehler steht, behält sein Präfix, etwa `Task failed: Action failed:`, und zeigt anstelle der Meldung `<error censored due to no log>`. Hier weicht Volant von `ansible-playbook` 2.19.12 ab, das nur den Grund eines nicht erreichbaren Hosts zensiert und die Meldung eines fehlgeschlagenen Tasks vollständig ausgibt, sodass ein Geheimnis in einer `fail`-Meldung das Terminal erreicht.

`no_log` kann ein Template sein. Jeder Host rendert es, wenn der Task läuft. In einer Schleife verbirgt ein einziges Element mit wahrem Wert alle Elemente, während ansible-core jedes Element nach seinem eigenen Wert verbirgt.

Das Banner des Tasks und der Name des Tasks werden nicht verborgen. Die registrierte Variable und der Recap behalten das echte Ergebnis, sodass ein späterer Task es noch lesen kann.

## Unterschiede zu Ansible

- `delegate_to` wird einmal pro Task gerendert, nicht einmal pro Schleifenelement. Ein Batch ist eine Nachricht an einen Agent über eine Verbindung, und ein Delegierter pro Element würde eine Schleife auf mehrere Verbindungen aufteilen.
- `local_action` ist `delegate_to: localhost` anders geschrieben und schlägt ein `delegate_to` am selben Task.
- Wenn der Host, der für einen `run_once`-Task gewählt wurde, nicht erreichbar ist, verlassen die wartenden Hosts das Play, und der Lauf endet dort. Ansible gibt noch ein weiteres leeres Play-Banner aus. Keine der Engines wählt einen anderen Host, und der Exit-Code ist derselbe 4.
- `run_once` zusammen mit `delegate_facts` speichert die Facts auf dem Delegierten, wodurch `delegate_facts` nur eines bedeutet. Das Verhalten von Ansible wurde hier nicht gemessen.
- `become`, `become_user` und `become_method`, die aus dem Schleifenelement geschrieben sind, werden für jedes Element gerendert. Elemente, die die Rechte gleich ausweiten, laufen über eine Verbindung; ein Element, das sie anders ausweitet, läuft über eine eigene, sodass `become: "{{ item }}"` über `[true, false]` das erste Element als root und das zweite als verbindender Benutzer ausführt, wie in Ansible. Eine solche Schleife führt ihre Elemente nacheinander aus.
- Ein `environment`-Wert, der kein Mapping ist, wird mit einer Warnung übersprungen, die den Wert nennt. Ansible gibt stattdessen den ganzen Stapel von Schichten aus, in dem er stand.
- Diese Warnung wird von `no_log` nicht zensiert. Ansible zensiert seine Warnung ebenfalls nicht, das entspricht also ansible-core, aber bedenken Sie es, bevor Sie ein Geheimnis in ein `environment` legen, das vielleicht kein Mapping ist.
