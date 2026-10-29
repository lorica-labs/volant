---
sourceHash: bd44eb3d24f2
title: Action-Plugins
description: Wie Volant die Module ausführt, die ansible-core über ein Action-Plugin auf dem Controller ausführt, in Rust oder auf der Python-Bridge.
---

Die meisten Module, die ansible-core mitbringt, laufen als sie selbst: Der Controller sendet eine Payload, der Host führt sie aus, das Ergebnis kommt zurück. Eine Handvoll läuft anders: ansible-core selbst entscheidet, was gesendet wird, manchmal erst nachdem es den Host etwas gefragt hat, und der Modulname am Task ist nur der Einstiegspunkt. `package` ist so eines: Was läuft, ist `apt` oder `dnf`, gewählt, nachdem der Host gesagt hat, welches er hat. `copy` ist ein weiteres: Ob überhaupt etwas zum Host reist, hängt von einer Prüfsumme ab, die der Host zuerst zurückmeldet.

Volant führt acht davon selbst aus: `copy`, `dnf`, `fetch`, `package`, `reboot`, `service`, `template` und `unarchive`. Jedes ist ein kleiner Zustandsautomat auf dem Controller: Er wird nach dem nächsten Schritt gefragt und bekommt das Ergebnis des letzten, bis er das eigene Ergebnis des Tasks hat. Jeder Schritt, den er macht, ist ein gewöhnliches Modul der Union des Laufs, gesendet über die Verbindung, die der Task schon hat, ein Schritt pro Hin- und Rückweg, außer `stat` und `file` von `copy`, wie unten beschrieben. Das Ergebnis, mit dem ein Plugin endet, geht denselben Weg wie jedes andere Modulergebnis, sodass nichts, was ein Host geantwortet hat, die Variablen eines Playbooks auf einem eigenen Weg erreicht.

Das Keyword `timeout:` eines Tasks begrenzt so das ganze Element, bei jedem der acht: jeden Teil-Task und jedes Warten darauf, dass der Host zurückkommt, das erneute Verbinden von `reboot` eingeschlossen. Läuft es ab, endet der Task dort mit `Task failed: Timed out after <seconds> second(s).`, nie mit dem, was das Plugin aus der verbleibenden Zeit gemacht hätte.

Die Namen, die ansible-core ebenfalls über ein Action-Plugin ausführt, und jedes Action-Plugin der Collections im Korpus laufen auf einem zweiten Weg: im eigenen Python des Plugins, von ansible-core, im Hilfsprozess des Controllers. Siehe [Auf der Bridge](#auf-der-bridge) unten. Siehe [Module](/de/docs/reference/modules/), was dieses Release nativ, auf dem Controller und auf dem warmen Python-Pfad ausführt.

## `copy`

Eine Controller-Datei oder ein `content:`, der direkt in das Ziel geschrieben wird, wird nur dann an den Host gesendet, wenn es sich unterscheidet.

Das Plugin führt zuerst `stat` auf dem Ziel aus und verlangt eine SHA-1, außer bei `force: false`: Dann verlangt es keine und lässt ein vorhandenes Ziel unberührt, was auch immer darin steht. Es vergleicht diese Summe mit der der lokalen Bytes. Ein Ziel, das schon dieselben Bytes enthält, bekommt `file`, um die Attribute zu setzen, die der Task verlangt hat; alles andere bekommt die Bytes auf dem Host bereitgestellt und einen Teil-Task `copy`. Nur dieser letzte Fall sendet etwas. Das `file` geht im selben Hin- und Rückweg wie das `stat`: Der Agent führt es nur aus, wenn das `stat` eine reguläre Datei mit dieser Summe gefunden hat, sodass ein Ziel, das die Bytes schon enthält, einen Hin- und Rückweg kostet. In einer Schleife über `copy` oder `template`, in der jedes Element ein eigenes `dest` nennt, ohne `until`, `break_when`, `pause`, `timeout`, `async` oder Delegation, machen die Elemente jeden Schritt gemeinsam, im selben Hin- und Rückweg: Eine Schleife über Dateien, die der Host schon hat, kostet einen Hin- und Rückweg.

`remote_src: true` überspringt all das: `copy` läuft allein, mit den eigenen Argumenten des Tasks, und `src` benennt eine Datei, die schon auf dem Host liegt.

Eine Datei über 16 MiB wird nie ganz gelesen: Das Plugin hasht sie auf dem Controller und schickt sie in Stücken von 16 MiB an den Host, die der Agent beim Eintreffen auf die Platte schreibt. `unarchive` schickt sein Archiv genauso.

Abgelehnt, bevor der Host etwas gefragt wird, in den eigenen Worten von ansible-core:

- `src (or content) is required`
- `dest is required`
- `src and content are mutually exclusive`
- `can not use content with a dir as dest`
- `copying a directory is not supported yet: <src>`. Das Rust-Plugin kopiert kein Verzeichnis, daher läuft der Task auf der Bridge, die es kann.
- Ein `src`, das auf dem Controller nichts benennt, verpackt als `Unexpected AnsibleActionFail error: Could not find or access '<src>' ...`.

## `template`

Ein Template, das auf dem Controller gerendert und dann demselben Pfad übergeben wird, den `copy` verwendet.

Das Plugin findet das Template unter `templates/`, rendert es einmal mit den Variablen des Tasks und seinen eigenen Trennzeichen und kopiert das Ergebnis so, wie `copy` einen `content:` kopiert: `stat`, dann `file` oder ein bereitgestelltes `copy`. Das Rendern geschieht genau einmal; nichts rendert das Ergebnis ein zweites Mal, sodass ein Wert, den ein verwalteter Host beigesteuert hat, als Text in der Datei landet und nicht als eigenes Template.

Vier Variablen werden für das Rendern gesetzt, zusätzlich zu denen des Tasks: `ansible_managed` (außer der Task hat sie schon gesetzt), `template_path` (`src`, wie der Task es geschrieben hat), `template_fullpath` (der auf dem Controller gefundene Pfad) und `template_destpath` (`dest`, wie gerendert, siehe unten). `template_host`, `template_uid`, `template_run_date` und `template_mtime`, die ansible-core ebenfalls setzt, werden nicht gesetzt.

`trim_blocks` ist standardmäßig an und `lstrip_blocks` standardmäßig aus, wie in ansible-core, und das abschließende Zeilenende der Datei bleibt erhalten. `newline_sequence` nimmt `\n`, `\r` oder `\r\n`, wobei die maskierte Schreibweise aus vier Zeichen genauso gelesen wird wie die wörtliche. Die sechs Trennzeichen-Optionen (`variable_start_string` und seine fünf Verwandten) und eine erste Zeile `#jinja2:` in der Template-Datei werden so gelesen, wie ansible-core sie liest, siehe [Eine Template-Datei rendern](/de/docs/variables/templating/#eine-template-datei-rendern). `output_encoding` schreibt UTF-8, UTF-8 mit Signatur, Latin-1, ASCII, cp1252, ISO 8859-15, UTF-16 und UTF-32 und benennt jede andere Kodierung als noch nicht unterstützt. Ein Template, das `{% include %}` oder `{% import %}` verwendet, schlägt fehl: Nichts hier lädt mitten im Rendern eine zweite Datei.

Abgelehnt, bevor der Host etwas gefragt wird:

- `'state' cannot be specified on a template`
- `src and dest are required`
- `newline_sequence needs to be one of: \n, \r or \r\n`
- `Block, variable and comment start strings must be different.`
- `TemplateOverrides.<name> must be <class 'str'> instead of <class 'NoneType'>`, bei einer Trennzeichen-Option, die leer oder null ist
- `output_encoding '<encoding>' is not supported yet on 'template': only utf-8, ... are`
- Ein `src`, das auf dem Controller nichts benennt, in den eigenen Worten von ansible-core, diesmal ohne `AnsibleActionFail`-Hülle.

## `package`

Der Paketmanager des Hosts benennt das Modul, das tatsächlich läuft.

Der Name kommt aus `use:`, außer es ist `auto`; dann aus der Variable `ansible_package_use` des Hosts; dann aus `ansible_facts.pkg_mgr` des Hosts, auf dem das Modul läuft; dann aus einem auf diesen einen Fact gefilterten `setup`, das für jeden Task neu läuft und nie behalten wird. Welches Modul der Name auch wählt, das läuft, mit `use` aus seinen Argumenten entfernt; ein fehlgeschlagenes `setup` beendet den Task mit `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`, und ein Name, der zu nichts aufgelöst wird, das dieses Release ausführen kann, schlägt fehl mit `Could not find a matching action for the "<name>" package manager.` Ein Name, den ein Host meldet, ist nie etwas anderes als ein Nachschlageschlüssel in dieser geschlossenen Liste: `setup`, `apt`, `dnf`, `dnf5`. Die Union trägt alle vier Backends und nicht nur das, das ein Lauf am Ende braucht, weil nach dem Bekanntwerden der Facts nichts mehr gebaut wird: Ein Plugin kann nur unter dem wählen, was schon gereist ist.

## `dnf`

Wieder der Paketmanager des Hosts, diesmal mit einer Wahl zwischen zwei Modulnamen statt vier.

Der Backend-Name kommt aus `use:` oder, wenn `use` fehlt, aus `use_backend:`; beide zusammen werden abgelehnt: `parameters are mutually exclusive: ('use', 'use_backend')`. `auto` und `yum` lesen `ansible_facts.pkg_mgr` des Hosts, auf dem das Modul läuft, gesammelt über ein auf diesen einen Fact gefiltertes `setup`, wenn er noch nicht bekannt ist; jeder andere Name wird wie angegeben genommen. `yum`, `yum4` und `dnf4` führen alle das Modul `dnf` aus, `dnf5` führt `dnf5` aus, und ein Name, der danach immer noch keiner davon ist, schlägt fehl mit der eigenen zweizeiligen Meldung von ansible-core, schließende Klammer und alles:

```text
Could not detect which major revision of dnf is in use, which is required to determine module backend.
You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})
```

Ein fehlgeschlagenes `setup` beendet den Task mit seiner eigenen Ursache, `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`. Wenn der Task nicht delegiert ist, trägt sein Ergebnis `ansible_facts.pkg_mgr` gesetzt auf das, was das `setup` geantwortet hat, so wie das eigene Ergebnis von ansible-core; das Ergebnis eines delegierten Tasks trägt keines, weil die Antwort eines Delegate sonst unter den Facts des falschen Hosts landen würde.

Das Modul `yum` ist in ansible-core 2.19 kein eigenes Modul: `yum`, `ansible.builtin.yum` und `ansible.legacy.yum` sind Weiterleitungen zur Action `dnf`, und ein Task, der eine davon nennt, führt dieses Plugin aus. Ein `module_defaults`-Schlüssel für `yum` gilt für einen `dnf`-Task und umgekehrt.

## `fetch`

Eine Datei, die von einem Host gelesen und unter `dest` auf den Controller geschrieben wird, und nirgendwo sonst.

Ohne `become` führt das Plugin zuerst `stat` auf der Quelle aus und lässt eine schon passende lokale Datei in Ruhe; unter `become` geht es direkt zu `slurp`. Die Bytes reisen immer über `slurp` zurück, strikt dekodiert, nie gerendert und nie in eine Variable gelegt. Das Ziel ist `dest/<inventory_hostname>/<src>`, außer `flat: true` benennt `dest` selbst direkt oder, mit einem Schrägstrich am Ende, `dest/<basename of src>`.

Gemessen an ansible-core 2.19.12 lässt ein relatives `src`, das mit `..` aufsteigt, ansible-core außerhalb von `dest` auf den Controller schreiben: Ein `src` von `../../../../../../../../tmp/x` ohne `flat` schreibt `/tmp/x`. Hier wird jeder Kandidatenpfad gebaut und normalisiert, ohne das Dateisystem anzufassen, und abgelehnt, wenn er nicht unter `dest` liegt, bevor überhaupt ein Verzeichnis angelegt wird:

- `the fetched path <p> is outside '<dest>'`

Das wird zweimal geprüft: einmal am `src`, das das Playbook geschrieben hat, und noch einmal am Pfad, den der Host zurückmeldet und der nicht übereinstimmen muss. Ist der Schutz an, wird ein `dest`, dessen Rendern von einem verwalteten Host gelesen hat, abgelehnt, bevor etwas gefragt wird, außer `[volant] host_named_dest_dirs` deckt es ab (siehe [Ein dest, das ein verwalteter Host benannt hat](#ein-dest-das-ein-verwalteter-host-benannt-hat)), und ebenso der `inventory_hostname` einer Schleife, wenn ein Host ihn gesetzt hat. Ein `inventory_hostname`, der nicht aus einer einzelnen Pfadkomponente besteht, wird immer abgelehnt:

- `the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it`
- `the inventory_hostname '<name>' is not one path component, and fetch files what it fetches under it`

Ein symbolischer Link irgendwo unterhalb von `dest` wird abgelehnt statt verfolgt, sowohl am aus dem Playbook gebauten Pfad als auch an dem, den der Host zurückmeldet.

`fail_on_missing`, wie in ansible-core standardmäßig an, lässt den Task bei einer fehlenden Quelle oder einem Verzeichnis fehlschlagen; `fail_on_missing: false` macht aus beidem ein `ok`-Ergebnis, das stattdessen die eigene Meldung von ansible-core trägt. In beiden Fällen lässt jeder andere Fehler den Task weiter fehlschlagen, etwa ein Lesen, das der Agent ablehnt, weil das Limit pro Stream des Modul-Servers erreicht wurde. Dieses Limit ist 4 MiB der Base64-kodierten Standardausgabe von `slurp`, etwa 3 MiB Quelldatei vor dem Kodieren. `validate_checksum` prüft die SHA-1 der geschriebenen Datei gegen die des Hosts, bevor sie behalten wird; bei Abweichung schlägt der Task fehl und entfernt, was geschrieben wurde, anders als ansible-core, das die abweichende Datei dort liegen lässt, wo es sie geschrieben hat. Eine Datei, die ersetzt wird, behält ihren bisherigen Modus, sodass eine erneut geholte `0600`-Datei `0600` bleibt.

### Ein dest, das ein verwalteter Host benannt hat

Ein `dest` wird von einem verwalteten Host benannt, wenn sein Rendern etwas gelesen hat, das ein Host beigesteuert hat. Ein echter Fall ist `dest: "{{ tmp.path }}/out/"`, wobei `tmp` das registrierte Ergebnis eines `tempfile`-Tasks ist. ansible-core schreibt dorthin, und `fetch` standardmäßig auch. Ein Host, der auf `tempfile` mit `/home/<operator>/.ssh` antwortet, lässt den Controller als der Operator unter diesem Verzeichnis schreiben. Dasselbe gilt für eine Schleifenvariable namens `inventory_hostname` über einer registrierten Liste, die das Verzeichnis unterhalb von `dest` benennt.

Zwei Einstellungen schalten den Schutz ein, für einen Lauf, in dem ein Host weniger vertrauenswürdig ist als die Person, die das Playbook ausführt. Die erste ist `[volant] host_named_dest_dirs` (Umgebungsvariable: `VOLANT_HOST_NAMED_DEST_DIRS`), eine durch Doppelpunkte getrennte Liste absoluter Verzeichnisse, standardmäßig nicht gesetzt. Die zweite ist `[volant] host_named_paths` mit dem Wert `contained` oder `refuse`, was wie eine leere Liste wirkt, wenn `host_named_dest_dirs` nicht gesetzt ist, sodass jedes solche `dest` abgelehnt wird. Eine gesetzte Liste gilt immer, was auch immer `host_named_paths` sagt, und ein leerer Wert lehnt jedes solche `dest` ab. Ist der Schutz an, wird ein `dest`, das ein Host benannt hat, nur geschrieben, wenn der Pfad, zu dem es im Dateisystem aufgelöst wird, strikt innerhalb eines der Verzeichnisse liegt:

- Das `dest` muss absolut sein und darf kein `..` enthalten. Ein relativer Pfad und ein Pfad, der mit `~` beginnt, werden abgelehnt, nie expandiert.
- Der tiefste Teil des Pfads, der existiert, wird über jeden Link aufgelöst, und der Rest wird angehängt. Ein Link, der das Verzeichnis verlässt, ein Link, der ins Leere zeigt, das Verzeichnis selbst und ein Geschwister, das seinen Namen als Präfix teilt (`/srv/fetch-evil` für `/srv/fetch`), werden abgelehnt. Die Prüfung läuft kurz vor dem Schreiben noch einmal.
- Das Schreiben geht vom Verzeichnis der Liste abwärts und lehnt jeden Link darunter ab, auch einen, der innen bleibt, und jedes vorhandene Verzeichnis darunter, das ein anderer Benutzer besitzt oder das jeder Benutzer ohne Sticky-Bit beschreiben kann. Ein anderer Benutzer könnte sonst eines von beiden zwischen Prüfung und Schreiben gegen einen Link tauschen. Die Verzeichnisse, die `fetch` auf dem Weg anlegt, gehören ihm selbst.
- Ein Verzeichnis der Liste, das die Gruppe oder andere Benutzer beschreiben können, nimmt nichts auf, außer sein Sticky-Bit ist gesetzt, wie bei `/tmp`. Das wird geprüft, wenn ein Task das Verzeichnis verwendet.
- Ein Eintrag, der relativ oder leer ist, mit `~` oder `..` geschrieben ist oder `/` lautet, stoppt den Lauf beim Start mit Exit-Code 5.
- Ein `dest`, das das Playbook geschrieben hat, unterliegt der Liste nicht, und die Einstellung ändert nichts für `src`, das eine Datei auf dem Host benennt.

```
the 'dest' of this task was named by a managed host, and '/home/op/.ssh/x/' is not inside any of the directories (host_named_dest_dirs = /srv/fetch)
```

Nennen Sie ein Verzeichnis, das nur der Operator beschreiben kann. Mit `/tmp` in der Liste kann ein Host darunter jeden Namen wählen.

Nur `fetch` unterliegt der Liste. `copy` und `template` mit `delegate_to: localhost` und `synchronize` im Pull-Modus schreiben ebenfalls ein `dest` auf den Controller. Sie werden nicht abgelehnt, wenn ein Host es benannt hat, wie in ansible-core und wie bisher.

## `service`

Das Init-System des Hosts benennt das Modul, genauso wie der Manager bei `package`: `use:` in Kleinbuchstaben, außer `auto`, dann `ansible_facts.service_mgr`, dann ein gefiltertes `setup` auf `ansible_service_mgr`. Anders als bei `package` fällt ein Name, den keines von `systemd`, `systemd_service`, `sysvinit` trägt, auf `service` zurück, statt den Task fehlschlagen zu lassen, und ebenso ein `setup`, das fehlgeschlagen ist. Wer `systemd` selbst nennt, wirft die Argumente weg, die dieses Modul nicht nimmt: `pattern`, `runlevel`, `sleep`, `arguments`, `args`, jedes mit eigener Warnung, `Ignoring "<name>" as it is not used in "systemd"`; ein Task, der es als `ansible.builtin.systemd` ausschreibt, behält sie.

Ein Lauf, der `package` und `service` nennt, trägt jedes Backend von beiden in der Union, bevor ein einziger Fact bekannt ist. Gemessen am eigenen Payload-Builder von ansible-core 2.19.12 sind das etwa 170 KB mehr, einmal pro Verbindung gesendet, als `apt` und `systemd_service` allein kosten würden.

## `unarchive`

Ein Archiv, das auf dem Controller gelesen wird oder schon auf dem Host liegt, entpackt vom Modul `unarchive`.

`copy:` geht in `remote_src` auf (die eigene Umkehrung von ansible-core), und beide zusammen werden abgelehnt: `parameters are mutually exclusive: ('copy', 'remote_src')`. Ein `creates:`, das auf dem Host schon existiert, überspringt den Task, geprüft mit einem `stat`, bevor `dest` überhaupt angesehen wird: `{"changed": false, "msg": "skipped, since <path> exists", "skipped": true}`; ein `stat`, das fehlschlägt, wird als „nicht da“ gelesen, so wie ein Shell-Existenztest einen Befehl läse, den er nicht ausführen konnte, und nicht als Task-Fehler. Das Ziel muss existieren und ein Verzeichnis sein, sonst schlägt der Task fehl mit `dest '<dest>' must be an existing dir`. Erst dann wird die Quelle gesucht, abgelehnt, wenn ein Host sie benannt hat, bereitgestellt und gesendet, außer `remote_src` benennt eine Datei, die schon dort liegt.

Abgelehnt, bevor der Host etwas gefragt wird:

- `src (or content) and dest are required`
- `a '~' path is not supported yet on 'unarchive'`, für `dest` oder `creates`. Der Task läuft dann auf der Bridge.
- Ein `src`, das auf dem Controller nichts benennt: `Task failed: Could not find or access '<src>' ...\nIf you are using a module and expect the file to exist on the remote, see the remote_src option`
- `src is a directory, not an archive: <src>`, für ein `src`, das auf dem Controller als Verzeichnis gefunden wird.

## `reboot`

Den Host neu starten und dann warten, bis er mit einer neuen Boot-ID antwortet und sein Testbefehl gelingt, bevor der Task endet.

Aus `plugins/action/reboot.py` von ansible-core 2.19.12 gelesen und daran gemessen, unter `become`, auf einem Host, der von selbst zurückkam: ein auf die Distribution gefiltertes `setup`, `cat /proc/sys/kernel/random/boot_id` als reiner Befehl, ein `find` über fünf Suchpfade nach dem Shutdown-Programm, der Shutdown-Befehl selbst, dann die Boot-ID erneut gelesen, bis sie sich ändert, und schließlich der Testbefehl. All das läuft über die bestehende Verbindung des Hosts, durch `raw`, so wie der Low-Level-Befehl von ansible-core über seine eigene läuft.

Gegen `local`, die Verbindung, die der Controller selbst verwendet, wird der Task vor allem anderen abgelehnt: `Running reboot with local connection would reboot the control node.`

Sobald der Shutdown-Befehl abgegangen ist, trennt dieses Release die Verbindung und öffnet eine frische, um nach dem Host zu sehen, statt die alte über den Neustart hinweg offen zu halten. `post_reboot_delay` ist, wie lange es vor der ersten Prüfung wartet, und `reboot_timeout` (standardmäßig 600 Sekunden) begrenzt das ganze Warten, außer das `timeout:` des Tasks kappt es früher. `connect_timeout`, wenn der Task eines angibt, setzt nur das SSH-`ConnectTimeout` jedes Wiederverbindens; es begrenzt den Versuch selbst nicht mehr. Eine leere Antwort auf die Boot-ID wird nicht als neuer Boot gelesen: Ein Host kann im Moment kurz vor dem Herunterfahren nichts antworten. Hat sich die Boot-ID geändert, muss `test_command` (standardmäßig `whoami`) noch gelingen, bevor der Task fertig ist; läuft eines der beiden Wartens ab, schlägt der Task fehl mit der eigenen Meldung von ansible-core, `Timed out waiting for <what> (timeout=<reboot_timeout>)`. Eine Verbindung, die ein anderer Host desselben Laufs zum gerade neu gestarteten offen gehalten hat, bekommt vor ihrer nächsten Verwendung eine zusätzliche Lebendprüfung und wird ersetzt, wenn der Neustart sie getötet hat: Dieser Treiber kappt nur seine eigenen Verbindungen, nicht die eines Delegate.

Die Shutdown-Zeile folgt der eigenen Tabelle von ansible-core: `shutdown -r <minutes> "<msg>"` auf den meisten Distributionen, nacktes `reboot` auf Alpine, `shutdown -r +<minutes> "<msg>"` auf Void, oder `reboot_command`, an seinem ersten Leerzeichen geteilt, wenn der Task eines angibt. `search_paths` (standardmäßig fünf Verzeichnisse) ist, wo das Shutdown-Programm gefunden wird, wenn `reboot_command` nicht schon einen absoluten Pfad nennt; wird es dort nicht gefunden, schlägt der Task fehl und nennt jeden durchsuchten Pfad.

Abgelehnt, bevor der Host etwas gefragt wird: Ein Argument außerhalb der eigenen neun von ansible-core (`boot_time_command`, `connect_timeout`, `msg`, `post_reboot_delay`, `pre_reboot_delay`, `reboot_command`, `reboot_timeout`, `search_paths`, `test_command`) lässt den Task fehlschlagen, sortiert und namentlich: `Invalid options for reboot: <name>[,<name>...]`.

## Auf der Bridge

`add_host`, `assemble`, `gather_facts`, `group_by`, `script`, `set_stats`, `uri` und `wait_for_connection` führen das eigene Action-Plugin von ansible-core im Hilfsprozess aus, und ebenso jedes Action-Plugin, das eine Collection mitbringt (ausgenommen die Plugins, die einen Windows-Host oder einen anderen Verbindungstyp brauchen; die Seite [Parität](/de/docs/reference/parity/) zählt sie). `copy` und `unarchive` nehmen denselben Weg für die beiden Formen, die die Rust-Plugins ablehnen: `copy` eines Verzeichnisses und `unarchive` mit einem `~` in `dest` oder `creates`. Unter `host_named_paths = refuse` nimmt ein `src`, das ein verwalteter Host benannt hat, ihn nie: Die Ablehnung des Rust-Plugins bleibt bestehen. `async_status` ist kein überbrücktes Plugin: Der Agent führt es aus, und ein überbrücktes Plugin fragt seine Jobs darüber ab.

Das Plugin fragt nach dem, was es braucht, und der Controller antwortet: dieses Modul ausführen, diesen Befehl ausführen, eine Variable lesen, ein temporäres Verzeichnis anlegen, diese Datei ablegen. Jede Antwort ist durch das `timeout:` des Tasks begrenzt. Ein Plugin, das nie zurückkehrt, oder ein Hilfsprozess, der stirbt, lässt diesen Task fehlschlagen und nicht den Lauf; ein temporäres Verzeichnis, das das Plugin auf dem Host zurückgelassen hat, wird entfernt. `add_host`, `group_by` und `set_stats` brauchen keinen Host und laufen ohne Verbindung. Was sie zurückgeben, wendet der Executor an: neue Hosts und Gruppen für die nächsten Tasks und die benutzerdefinierten Statistiken, die nach dem Recap ausgegeben werden. Ein Wert von `add_host`, der aus einem Fact gerendert wurde, bleibt Daten: Er wird als Fact des neuen Hosts behalten, nie als Inventory-Variable. Dasselbe gilt für einen Host- oder Gruppennamen von `add_host` oder `group_by`, der aus Text gebaut wurde, den ein verwalteter Host geschrieben hat. Dasselbe gilt für jeden Host- oder Gruppennamen, der `{` enthält, ob die Inventory-Dateien, eine dynamische Inventory-Quelle, `meta: refresh_inventory`, `add_host` oder `group_by` ihn geliefert haben: `groups`, `hostvars`, `group_names`, die Hostlisten eines Plays und `inventory_hostname` zeigen ihn als Text, wie ansible-core es tut. Sobald ein solcher Name existiert, ist jedes Lesen der Listen, die ihn tragen, Daten. Einen Host über `hostvars[name]` zu lesen ist nur für diesen Host Daten, ein verschachteltes Template eines anderen Hosts wird also weiterhin gerendert.

Der Hilfsprozess steht für die Verbindung des Controllers, daher werden einige Dinge, die ansible-core einem Plugin erlaubt, namentlich abgelehnt, bevor sie etwas tun:

- Ein Mitglied der Verbindung oder des Play-Kontexts, das nicht zu den wenigen gehört, die der Shim trägt: `Connection.<name> is not supported yet`, `the connection option <name> is not supported yet`, `the play context field <name> is not supported yet`, `the shell plugin <name> is not supported yet`.
- Ein `async_status`-Modus außer `status` und `cleanup`.
- Ein Modul, das kein UTF-8-Text ist, wo der Shim es als Text liest.
- Die Task-Keywords `action`, `local_action` und `module_defaults` im Hilfsprozess: Der Controller hat die Defaults schon zusammengeführt.

Zwei Teile der Verbindung des Hilfsprozesses folgen ansible-core:

- `wrap_async` startet einen echten Job auf dem Agent. Jobs laufen nebeneinander, und ein Job, der sein `async:`-Limit überschreitet, wird getötet und endet mit `Timeout exceeded`.
- Ein Befehl, den ein Plugin mit `sudoable=False` sendet, das temporäre Verzeichnis des Plugins und die Dateien, die es ablegt, laufen als der Verbindungsbenutzer, über eine zweite Verbindung ohne Rechteausweitung. Unter `become` laufen nur `sudoable`-Befehle und Module mit erweiterten Rechten.

### Ein Plugin, das seine eigene Verbindung aufbaut

`ansible.posix.synchronize` über ssh führt sein Modul nicht auf dem Host aus. Es baut eine eigene `local`-Verbindung auf und führt das Modul auf dem Controller aus, wo `rsync -e ssh` die Dateien zum oder vom Host kopiert. Der Hilfsprozess gibt einem solchen Plugin die eigenen Methoden von ansible-core zum Ausführen eines Moduls und hält seine Stellvertreter-Verbindung für die Verbindung bereit, mit der der Task kam.

Das Plugin liest Schlüssel und Benutzer der Verbindung (die Hostvariable, `--private-key`, `-u` oder `ansible.cfg`), ob der Task die Rechte ausweitet und als wen (`sudo` wird zu `--rsync-path='sudo -u <user> rsync'`), `ansible_port`, `delegate_to` und `ansible_delegated_vars` und, mit `use_ssh_args`, `ansible_ssh_common_args` und `ansible_ssh_extra_args` des Hosts. `ssh_args` ist der Standard von ansible-core, `-C -o ControlMaster=auto -o ControlPersist=60s`: Ein in `ansible.cfg` gesetztes `ssh_args` wird nicht gelesen. `rsync` muss auf dem Controller und auf dem Host vorhanden sein. Push, Pull, `delete`, `rsync_opts`, `delegate_to`, `become` mit `sudo`, `--check` und `--diff` ergeben die Ausgabe, die `ansible-playbook` gibt.

### Bekannte Lücken

- `wait_for_connection` auf einem Host, der von der ersten Verbindung an down ist, endet mit `UNREACHABLE` für ein Plugin-Element, das nie eine Verbindung bekommen hat; ansible-core meldet `failed` mit `timed out waiting for ping module test`.
- Ein überbrücktes Ergebnis, das auf Ergebnisebene `unreachable` ist, wird als `ok` ausgegeben.
- Ein übersprungenes überbrücktes Ergebnis trägt `"failed": false`.
- Die Warnungen, die ein Plugin mit `display` schreibt, gehen an die Standardfehlerausgabe des Hilfsprozesses, nicht an die Ausgabe des Tasks; ebenso die Warnung `Error loading plugin` von ansible-core für einen unbekannten Collection-Namen.
- Ein Modul, das `runtime.yml` an ein anders benanntes Action-Plugin weiterleitet, läuft unter dem eigenen Namen des Tasks.
- `ansible_play_hosts_all` wächst nicht für einen Host, der innerhalb eines `serial`-Plays hinzugefügt wird, und ein delegiertes `add_host` oder `group_by` gilt für den eigenen Host des Tasks.

## Eine Quelldatei, die ein verwalteter Host benannt hat

Ein `src` wird von einem verwalteten Host benannt, wenn sein Rendern etwas gelesen hat, das ein Host beigesteuert hat: ein gesammelter Fact, wie in `src: "{{ ansible_os_family }}.j2"`, ein registriertes Ergebnis oder ein Element von `with_first_found`, das aus einem davon gebaut wurde. `[volant] host_named_paths` legt fest, was `copy`, `template`, `script`, `unarchive` und `assemble` damit tun. Es gilt für die Datei, die auf dem Controller gelesen wird, und auch unter `--check` und `--diff`. Bei `template` wird eine Trennzeichen-Option (`variable_start_string` und die fünf anderen), deren Wert ein Host beigesteuert hat, unter `allow` verwendet, wie ansible-core sie verwendet, und unter `contained` und `refuse` abgelehnt. Eine erste Zeile `#jinja2:` im Template eines Autors wird in jedem Modus gelesen.

| Wert | Was der Task tut |
|---|---|
| `allow` (der Standard) | Sucht die Datei und sendet sie, wie ansible-core es tut. |
| `contained` | Liest die Datei nur, wenn sie in einem Verzeichnis `files` oder `templates` liegt, das der Task durchsucht. |
| `refuse` | Lässt den Task fehlschlagen, bevor ein Pfad nachgeschlagen wird. |

`allow` bedeutet, dass ein verwalteter Host wählen kann, welche lesbare Controller-Datei ein Task `copy`, `template` oder `script` ihm sendet, genau wie in ansible-core 2.19.12. Dort gemessen sendet ein `src`, das ein registrierter Wert, die `stdout` eines Befehls oder ein Fact ist, jede Datei, die es benennt, `~/.ssh` eingeschlossen. Rollen, die ein Template nach Fact wählen, hängen davon ab. `contained` und `refuse` gibt es, um das zu verschärfen, für einen Lauf, in dem ein Host weniger vertrauenswürdig ist als die Person, die das Playbook ausführt.

Unter `contained` zählen die Verzeichnisse `files` und `templates` der Rolle, der Rollen, von denen sie eine Abhängigkeit ist, des Verzeichnisses der Datei des Tasks (`tasks/files` in einer Rolle) und des Playbook-Verzeichnisses. Der Rest dieser Bäume zählt nicht: `group_vars`, `vars`, `defaults`, ein Inventory, `ansible.cfg` und eine Vault-Passwortdatei sind nie über einen Namen erreichbar, den ein Host gewählt hat, und ebenso wenig eine Datei neben dem Playbook. Die Regeln, der Reihe nach:

- Ein Name, der mit `~` oder `~user` beginnt, wird abgelehnt. Die eigenen Plugins von ansible-core expandieren ihn zu einem Home-Verzeichnis, das der Text des Namens nicht zeigt.
- Ein Name mit einer `..`-Komponente wird abgelehnt, auch wenn sie nach innen zurückfaltet. Das Dateisystem folgt einem Link, bevor es `..` faltet, der gefaltete Pfad ist also nicht der, der geöffnet würde.
- Ein absoluter Name muss unter einem dieser Verzeichnisse liegen und darf kein `$` enthalten, das ansible-core in einem absoluten Namen expandiert. `with_first_found` liefert einen absoluten Namen dieser Art.
- Ein relativer Name wird so nachgeschlagen, wie das Plugin ihn nachschlägt. Die erste Datei, die existiert, ist die gelesene, und sie muss unter einem dieser Verzeichnisse liegen. Eine Datei dieses Namens neben dem Playbook, außerhalb von `files`, wird abgelehnt, wenn sie existiert. Dadurch verrät die Ablehnung einem Host, dass eine solche Datei existiert. Das ist begrenzt auf eine Datei bei `<playbook>/<name>`, `<role>/<name>`, `<role>/tasks/<name>` oder `<parent role>/<name>`; die Antwort für jeden anderen Namen ist das übliche „could not find“.

Die Prüfung liest den Text des Pfads. Ein Link, den Sie in diese Verzeichnisse gelegt haben, ist Ihre eigene Datei und wird verfolgt. Eine Quelle auf dem Host, etwa `remote_src: true` bei `copy` und `unarchive` oder `assemble`, wenn `remote_src` nicht auf false steht, ist keine Controller-Datei und wird nicht geprüft. `template`, `script` und die Plugins einer Collection lesen kein `remote_src`, es nimmt sie also nie aus. Bei `script` ist die Datei das erste Wort der Zeile oder von `cmd`. Bei `uri` zählen `src` und der `filename` jedes Felds eines mehrteiligen `body`.

Ein Task wird abgelehnt, bevor der Pfad nachgeschlagen wird, wenn der Modus oder der Text des Namens es entscheidet, weil eine Antwort „nicht gefunden“ einem Host schon verraten würde, ob ein Pfad auf dem Controller existiert. Unter `refuse` lautet die Meldung:

```text
the 'src' of this task was named by a managed host, and a controller file a host chose is
never sent
```

Unter `contained` nennt sie den Wert und die Einstellung:

```text
the 'src' of this task was named by a managed host, and '../up.j2' is not inside the
files or templates directories of the playbook or the role (host_named_paths = contained)
```

Das Argument ist `_raw_params` oder `cmd` bei einem `script`. Jedes andere Wort für die Einstellung stoppt den Lauf beim Start mit Exit-Code 5 und nennt den Schlüssel, die drei Wörter und den gefundenen Wert.

Die Einstellung deckt die oben aufgeführten Tasks ab. Sie deckt weder `include_vars`, `vars_files` noch ein Lookup wie `file` ab, dessen Pfad ein Host benannt hat. Auch ein `copy` mit `remote_src` oder ein an `localhost` delegiertes `slurp`, das einen Controller-Pfad liest, den ein Host benannt hat, deckt sie nicht ab. Diese verhalten sich wie in ansible-core, und genauso in jeder früheren Version von Volant.

Der Modus ändert, welche Datei gelesen wird, nie, wie dem Text eines Templates vertraut wird. Wenn das `src` eines `template` von einem Host benannt wurde, sind `template_path` und `template_fullpath` im Render die Werte dieses Hosts, so wie `template_destpath` es für ein `dest` ist.

Ein `fetch`, dessen `dest` ein Host benannt hat, folgt ebenfalls dieser Einstellung, obwohl sein `dest` ein Ort ist, an den der Controller schreibt, und keine Datei, die er liest: `allow` schreibt es, und `contained` und `refuse` schreiben es nur innerhalb von `host_named_dest_dirs` (siehe [Ein dest, das ein verwalteter Host benannt hat](#ein-dest-das-ein-verwalteter-host-benannt-hat)). Das `dest` der anderen Plugins ist immer nur ein Ort zum Schreiben auf dem Host und wird daher nicht abgelehnt; `template_destpath` trägt es als eigenen Wert des Hosts in das Rendern.

## Was auf dem Host bleibt

Eine Datei reist nur dann zu einem Host, wenn ein Teil-Task sie wirklich braucht, und nie weiter als bis zu diesem Task.

Der Controller stellt sie als inhaltsadressierten Blob bereit, benannt nach dem BLAKE3-Hash seiner Bytes, in einem Verzeichnis, das der einen Verbindung vorbehalten ist, die ihn gesendet hat: `stage-<host>-<pid>/`, im eigenen Cache des Agents, Modus 0700 und abgelehnt, wenn der Agent es nicht allein besitzt. Das Modul liest die Datei von dort, unter einem Namen, den der Agent selbst wählt, nie unter dem Pfad des Controllers. Sobald der Teil-Task, der sie brauchte, gelaufen ist, ob er gelungen oder fehlgeschlagen ist oder das Modul die Datei selbst schon verbraucht hat (wie `copy`, wenn es seine bereitgestellte Quelle an ihren Platz verschiebt), entfernt der Agent, was davon übrig ist. Nichts wartet auf das Ende der Verbindung: Ein gerendertes Template trägt oft ein Geheimnis, und eine Datei, nach der niemand ein zweites Mal gefragt hat, bleibt nicht liegen, damit sie später jemand findet.

Das ganze Bereitstellungsverzeichnis der Verbindung verschwindet ebenfalls, sobald die Verbindung selbst endet, was auch immer darin geblieben ist. Ein neuer Agent, der auf einem Host startet, entfernt außerdem die Bereitstellungsverzeichnisse toter Agents desselben Hosts, die er beim Start findet, sodass ein unterbrochener Lauf sie nicht für immer zurücklässt. Zwei Verbindungen zu einem Host, etwa zwei Inventory-Namen für eine Maschine oder zwei Hosts, die an denselben delegieren, teilen nie ein Bereitstellungsverzeichnis, da jedes nach seiner eigenen Prozess-ID benannt ist: Die bereitgestellte Datei einer Verbindung ist nie eine, die eine andere Verbindung sehen kann.
