---
sourceHash: 8ebece6e812f
title: Verbindungen und become
description: Wie Volant einen Host erreicht, welche Hostvariablen und ansible.cfg-Einstellungen es liest und wie become über sudo und su funktioniert.
---

Diese Seite beantwortet, wie Volant Ihre Hosts erreicht, welche Variablen die Verbindung steuern und wie Tasks mit `become` als anderer Benutzer laufen.

Volant erreicht einen verwalteten Host, legt dort einen kleinen statischen Agent ab (ein Programm, das Ihre Tasks ausführt und das Ergebnis meldet) und spricht für den Rest des Laufs über dieselbe Verbindung mit ihm.

## Verbindungstypen

`ansible_connection` wählt den Transport. Wie in Ansible ist `ssh` der Standard.

| Wert | Was er tut |
|---|---|
| `ssh` | Startet den OpenSSH-Client aus Ihrem `PATH`. Volant baut die Kommandozeile selbst, ohne Wrapper und ohne Rust-SSH-Bibliothek, sodass Ihre `~/.ssh/config` gilt. |
| `local` | Startet den Agent als Kindprozess auf dem Controller, ohne Netzwerk. |
| `docker` | Startet den Agent über `docker exec -i` in einem laufenden Container, wie `community.docker.docker`. Siehe [Docker-Container](#docker-container). |

Jeder andere Name wird vom ansible-core des Controllers aufgelöst, so wie `ansible-playbook` ihn auflöst, einmal pro Name und Lauf. `ansible.builtin.ssh` und `ansible.builtin.local` lösen sich zu den beiden Plugins oben auf und laufen. Ein kurzer Name, den eine Collection von ansible-core übernommen hat, wird zu dieser Collection geleitet, wenn sie installiert ist:

| Kurzer Name | Löst sich auf zu | Collection im Paket `ansible` 12.3.0 |
|---|---|---|
| `docker` | `community.docker.docker` | community.docker 4.8.5 |
| `podman` | `containers.podman.podman` | containers.podman 1.18.0 |
| `buildah` | `containers.podman.buildah` | containers.podman 1.18.0 |
| `kubectl` | `kubernetes.core.kubectl` | kubernetes.core 6.2.0 |
| `lxd` | `community.general.lxd` | community.general 11.4.2 |
| `chroot` | `community.general.chroot` | community.general 11.4.2 |
| `aws_ssm` | `community.aws.aws_ssm`, das `community.aws` 10.0.0 an `amazon.aws.aws_ssm` weiterleitet | amazon.aws 10.1.2 |
| `incus` | kein kurzer Name: `community.general.incus` schreiben | community.general 11.4.2 |

Mit `-vvv` gibt ein umgeleiteter Name `redirecting (type: connection) ansible.builtin.chroot to community.general.chroot` unter dem ersten Task aus, der ihn auf einem Host verwendet, eine Zeile pro Schritt.

Ein Name, der sich zu einem Plugin auflöst, das Volant noch nicht steuert, etwa `winrm`, `psrp` oder `containers.podman.podman`, macht den Host nicht erreichbar, mit `connection '<full name>' is not supported`, und der übrige Lauf geht weiter. Die [Unterstützungsmatrix](/de/docs/reference/parity/#collection-connection) listet jedes Verbindungs-Plugin der Collections im Paket auf.

Ein Name, der sich zu nichts auflöst, lässt den Task mit `the connection plugin '<name>' was not found` fehlschlagen, wie bei Ansible. Dazu gehört ein kurzer Name, der zu einer nicht installierten Collection geleitet wird, etwa `docker` ohne community.docker, nach der Warnung, die ansible-core dafür ausgibt, und `paramiko`, das ansible-core 2.19.12 unter diesem Namen nicht mitbringt. Das gilt unabhängig davon, ob der Wert die eigene Verbindung des Hosts ist, die eines `delegate_to`-Ziels oder die des Hosts, der einen `run_once`-Task ausführt.

Ein Passwort kommt aus `-k`, aus `--connection-password-file` oder aus der Variable `ansible_password`. Volant übergibt es `ssh` über `SSH_ASKPASS` und eine private temporäre Datei, die am Ende des Laufs entfernt wird; das setzt OpenSSH 8.4 oder neuer voraus. `ssh` läuft dann mit `BatchMode=no` und `NumberOfPasswordPrompts=1`, sodass eine falsche Antwort den Versuch beendet. Das Passwort steht nie in einer Kommandozeile, in der Umgebung eines Programms oder in der Ausgabe, auch nicht bei `-vvvv`. Ein per Vault verschlüsseltes `ansible_password` funktioniert wie jede verschlüsselte Variable. Ohne Passwort läuft `ssh` mit `BatchMode=yes` und fragt nie nach; ein Host, der nach einem Passwort oder einer Passphrase fragen würde, wird deshalb als nicht erreichbar gemeldet, statt den Lauf zu blockieren.

## Docker-Container

`ansible_connection: docker` oder `community.docker.docker` führt ein Play in einem laufenden Container aus, ohne sshd darin und ohne Netzwerkweg dorthin. Beide Namen brauchen community.docker auf dem Controller, wie bei `ansible-playbook`. Volant startet das Programm `docker` aus Ihrem `PATH`, sodass `DOCKER_HOST`, `DOCKER_CONTEXT` und `DOCKER_CONFIG` es über die Umgebung erreichen.

Der Agent kommt über `docker exec -i` hinein, daher braucht das Image `/bin/sh` und die üblichen Werkzeuge: `uname`, `id`, `ls`, `mkdir`, `df`, `awk`, `head`, `wc`, `chmod`, `mv` und `find`. Ein minimales Alpine- oder Debian-Image hat sie. Ein Image ohne Shell, etwa distroless, bekommt den Agent stattdessen über `docker cp`: Volant fragt `docker inspect` nach der Architektur des Images, kopiert den Agent in das `/tmp` des Containers, und der Agent legt sich selbst in sein Cache-Verzeichnis, mit denselben Prüfungen. Der Container braucht ein `/tmp`, `remote_tmp` muss absolut sein oder für einen Benutzer mit Home-Verzeichnis mit `~` beginnen, und `become` wird abgelehnt, weil keine Shell da ist, die `sudo` starten könnte. Die Kopie in `/tmp` bleibt, wenn der Benutzer des Containers sie nicht entfernen kann.

| Variable | Wirkung |
|---|---|
| `ansible_docker_host` | Name oder ID des Containers. Sie hat Vorrang vor `ansible_host`, das Vorrang vor dem Inventory-Namen hat. |
| `ansible_docker_user` | Der Benutzer, unter dem die Befehle laufen, `docker exec -u`. Sie hat Vorrang vor `ansible_user`, dem Schlüsselwort `remote_user` und `-u`. Nicht gesetzt, der eigene Benutzer des Containers. |
| `ansible_docker_extra_args` | Argumente nach `docker` und vor `exec`, an einzelnen Leerzeichen getrennt und nicht wie Shell-Wörter. Der Standard kommt aus `[docker_connection] extra_cli_args`. |
| `ansible_docker_extra_env` | Eine Zuordnung von Variablen, die mit `docker exec -e` gesetzt werden. Jeder Wert muss Text sein: eine Zahl oder ein Boolescher Wert ohne Anführungszeichen wird abgelehnt. |
| `ansible_docker_privileged` | `docker exec --privileged`. Der Standard kommt aus `ANSIBLE_DOCKER_PRIVILEGED` oder `[docker_connection] privileged`. |
| `ansible_docker_working_dir` | `docker exec -w`. Der Standard kommt aus `ANSIBLE_DOCKER_WORKING_DIR` oder `[docker_connection] working_dir`. |
| `ansible_docker_timeout` | Sekunden, die `docker` für die Antwort auf den ersten Befehl haben darf. Der Standard kommt aus `ANSIBLE_DOCKER_TIMEOUT`, dann aus `[docker_connection] timeout`, dann aus dem Verbindungs-Timeout. |

`become` mit `sudo` läuft über das `sudo` des Containers. `su` wird abgelehnt, weil `docker exec` ihm kein Terminal gibt. Ein Container, der nicht existiert oder angehalten ist, oder ein Benutzer, den das Image nicht hat, macht den Host nicht erreichbar, mit der eigenen Meldung von docker. ansible-core meldet alle drei Fälle als `Failed to create temporary directory`, diese Meldung weicht also absichtlich ab.

Mit `-vvvv` zeigen die Zeilen `ESTABLISH DOCKER CONNECTION FOR USER: <user>`, dann eine `EXEC [...]`-Zeile pro Befehl, die Volants eigene Befehle sind. Nennt das Play keinen Benutzer, kommt er aus `docker inspect`: `root`, wenn das Image keinen nennt, `?`, wenn docker es nicht sagen kann.

## Hostvariablen

Volant liest diese pro Host, aus dem Inventory oder aus jeder anderen Variablenquelle. Ein Test gleicht diese Liste mit den Namen ab, die der Verbindungscode liest, sodass sie mit der Engine übereinstimmt.

| Variable | Wirkung |
| --- | --- |
| `ansible_connection` | `ssh` (Standard) oder `local` |
| `ansible_host`, `ansible_ssh_host` | Adresse, zu der verbunden wird. Standard ist der Inventory-Name. Ein Wert, der mit `-` beginnt oder Leerraum oder ein Steuerzeichen enthält, wird abgelehnt, bevor `ssh` läuft. |
| `ansible_port`, `ansible_ssh_port` | `ssh -p`. Eine Zahl in Anführungszeichen funktioniert wie eine einfache. |
| `ansible_user`, `ansible_ssh_user` | `ssh -l` |
| `ansible_password`, `ansible_ssh_pass`, `ansible_ssh_password` | Das Verbindungspasswort, wie oben beschrieben an `ssh` übergeben. |
| `ansible_private_key_file`, `ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_timeout`, `ansible_ssh_timeout` | Sekunden, die `ssh` auf die Verbindung wartet, `ssh -o ConnectTimeout`. |
| `ansible_ssh_executable` | Das auszuführende `ssh`-Programm. Standard ist `ssh` aus Ihrem `PATH`. |
| `ansible_ssh_common_args` | Zusätzliche `ssh`-Argumente, so zerlegt, wie eine Shell sie zerlegen würde |
| `ansible_ssh_extra_args` | Dasselbe, nach den allgemeinen angehängt |
| `ansible_ssh_use_tty` | `raw`- und `script`-Befehle auf einem Terminal ausführen, wie es `ssh -tt` tut. An, solange es nicht auf false steht. Der Standard kommt aus [`usetty`](/de/docs/reference/configuration/#ssh-connection-usetty). |
| `ansible_remote_tmp` | Verzeichnis, das den Agent-Cache auf diesem Host enthält |
| `ansible_become` | Rechte ausweiten oder nicht, hat Vorrang vor dem Keyword `become` |
| `ansible_become_user`, `ansible_sudo_user`, `ansible_su_user` | Zielbenutzer der Rechteausweitung |
| `ansible_become_method` | `sudo` oder `su` |
| `ansible_become_password`, `ansible_become_pass`, `ansible_sudo_pass`, `ansible_su_pass` | Passwort für die Rechteausweitung |
| `ansible_become_exe`, `ansible_sudo_exe`, `ansible_su_exe` | Das Programm für die Rechteausweitung |
| `ansible_become_flags`, `ansible_sudo_flags`, `ansible_su_flags` | Dessen Flags |
| `ansible_su_prompt_l10n` | Eine Liste von Eingabeaufforderungen, die `su` ausgeben darf. Sie ersetzt die eingebauten. |

Setzt ein Host eine Variable und einen Alias davon, gilt der, den Ansible gewinnen lässt. Die Tabelle führt sie von der stärksten zur schwächsten auf, mit dem Keyword, das dasselbe setzt:

| Einstellung | Variablen | Keyword |
|---|---|---|
| Adresse | `ansible_ssh_host`, `ansible_host` | |
| Port | `ansible_ssh_port`, `ansible_port` | `port` |
| Benutzer | `ansible_ssh_user`, `ansible_user` | `remote_user` |
| Schlüsseldatei | `ansible_ssh_private_key_file`, `ansible_private_key_file` | |
| Timeout | `ansible_ssh_timeout`, `ansible_timeout` | |
| Passwort | `ansible_ssh_password`, `ansible_ssh_pass`, `ansible_password` | |
| Verbindung | `ansible_connection` | `connection` |
| Benutzer, Programm und Flags der Rechteausweitung | `ansible_sudo_*` oder `ansible_su_*` (je nach Methode), dann `ansible_become_*` | `become_user`, `become_exe`, `become_flags` |
| Passwort der Rechteausweitung | `ansible_sudo_pass` oder `ansible_su_pass`, dann `ansible_become_pass`, dann `ansible_become_password` | |

Eine Passwortvariable schlägt `-k` und `--connection-password-file`, und eine Variable mit leerem String bietet gar kein Passwort an. Eine Verbindungsvariable wirkt auf jeder Ebene, die Variablen setzt: Inventory, `group_vars`, `vars` eines Plays, ein Block, `vars` eines Tasks und `-e`.

Die Keywords gelten an einem Play, einem Block und einem Task, und `-c` legt die Verbindung für Hosts fest, die keine nennen. Die Reihenfolge ist die von Ansible: Eine Hostvariable wie `ansible_user` gewinnt, dann der Task, der Block und das Play, dann die Kommandozeile und die `ansible.cfg`. Facts, die ein Host über sich selbst meldet, wählen nie seinen Benutzer, Port oder seine Verbindung. Ein Keyword, das als Template geschrieben ist, wird einmal pro Task gerendert. Diese Werte lassen den Task fehlschlagen: ein Benutzername, der mit `-` beginnt oder ein Leerzeichen, ein Anführungszeichen oder ein Steuerzeichen enthält, ein Port außerhalb von 1-65535 und ein Verbindungsname mit etwas anderem als Buchstaben, Ziffern, `_` und `.`. Ein delegierter Task nutzt `remote_user` und `port` des Tasks, es sei denn, das Delegationsziel setzt eigene `ansible_user` oder `ansible_port`.

Ein nicht geschlossenes Anführungszeichen in `ansible_ssh_common_args` oder `ansible_ssh_extra_args` lässt den Host fehlschlagen, statt verworfen zu werden. Das ist hinter einem Bastion-Host wichtig: Würde ein `ProxyCommand` stillschweigend verworfen, ginge die Verbindung direkt an eine Adresse, die zu einem anderen Rechner gehören kann.

Die `ansible.cfg`-Einstellungen, die Verbindungen betreffen, etwa `timeout`, `remote_user`, `host_key_checking` und `forks`, stehen unter [Konfiguration](/de/docs/reference/configuration/).

## Der Agent-Cache

Der Agent ist ein kleines statisches Programm, für x86_64 Linux etwa 1,2 MB groß. Volant lädt ihn einmal pro Host hoch und verwendet ihn bei jedem späteren Lauf wieder, sodass ein Host nur den Agent und, wenn Python-Module liefen, einen Payload-Cache enthält.

Er liegt unter `<remote_tmp>/volant-agent-<version>-<uid>/volant-agent`, mit dem Standard-`remote_tmp` also unter `~/.ansible/tmp/` im Home-Verzeichnis. Die Version im Pfad ist die des Controllers selbst; so vermeidet ein aktualisierter Controller, einen älteren Agent auszuführen. Ein Task mit Rechteausweitung führt seinen eigenen Agent als `become_user` aus, im Cache im Home-Verzeichnis dieses Benutzers, sodass ein Host einen Cache pro Konto hält, als das der Lauf handelt.

Sind Python-Module gelaufen, liegt neben dem Agent-Verzeichnis im selben `remote_tmp` ein zweites Verzeichnis, `volant-blobs-<version>-<uid>`, mit den Modul-Payloads, die Volant gesendet hat.

Wie der Agent dorthin kommt, hängt vom Ziel ab. Über `ssh` und in einem Container mit Shell wird der Agent einem Shell-Skript auf der Verbindung übergeben, das das Verzeichnis prüft und die Datei schreibt. In einem Container ohne Shell legt der eigene Kopierbefehl der Laufzeit den Agent hinein, und der Agent legt sich mit denselben Prüfungen selbst ab; docker macht das mit `docker cp`.

Um beide zu leeren, entfernen Sie die beiden Verzeichnisse. Sonst bleibt nichts zurück:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-agent-* ~/.ansible/tmp/volant-blobs-*'
```

[Der Agent auf Ihren Hosts](/de/docs/internals/agent/#der-agent-cache) behandelt das Hochladen, die Reparatur eines beschädigten Caches und die Berechtigungsprüfungen.

## Dauerhafte Verbindungen

Ein Host behält seine Verbindung zum Agent für den ganzen Lauf, über Plays hinweg. Tasks laufen über diese eine Verbindung, statt jedes Mal eine neue zu öffnen. Bricht die Verbindung zwischen zwei Plays ab, verbindet Volant sich einmal neu, und ein zweiter Fehlschlag meldet den Host als nicht erreichbar.

Manche Tasks fügen eine Verbindung hinzu:

- Rechteausweitung. Ein Host, der `become` nutzt, hält eine zweite Verbindung zu einem Agent, der als Zielbenutzer läuft. Sie bleibt bis zum Ende des Plays bestehen, sodass ein Play mit Tasks mit Rechteausweitung einen Agent mit erweiterten Rechten startet, nicht einen pro Task. Ein Host hält höchstens eine solche Verbindung gleichzeitig: Der Wechsel zu einem anderen Benutzer ersetzt sie.
- Delegation. Ein delegierter Task läuft über die eigene Verbindung des Delegationsziels, mit dessen Einstellungen. Ein Host, dessen Play an drei andere Hosts delegiert, hält diese drei Verbindungen neben seiner eigenen.

## Eine Verbindung pro Host

Alle `ssh`-Aufrufe, die Volant für einen Inventory-Host macht, teilen sich eine einzige OpenSSH-Verbindung, sodass nur der erste Aufruf den Schlüsselaustausch und die Authentifizierung bezahlt. Die gemeinsame Verbindung bleibt nach ihrer letzten Sitzung 30 Sekunden offen. OpenSSH erledigt die Arbeit, daher gelten `~/.ssh/config`, `Match`, `Include`, `ProxyJump` und Ihr ssh-agent genau wie bei `ansible-playbook`.

Volant überlässt das Teilen Ihnen, wenn Ihre eigenen `ssh`-Argumente oder Ihre Konfiguration für den Host bereits `ControlMaster`, `ControlPath` oder `ControlPersist` setzen. Mit `[volant] ssh_control_master = false` oder `VOLANT_SSH_CONTROL_MASTER=0` schalten Sie das ab, siehe [Konfiguration](/de/docs/reference/configuration/#volant-ssh-control-master).

Jede Verbindung kostet den Controller drei Dateideskriptoren. Ein hartes Limit von 1024 offenen Dateien stoppt einen Lauf bei etwa 330 Verbindungen oder etwa 169 Hosts mit Rechteausweitung. [Der Agent auf Ihren Hosts](/de/docs/internals/agent/#eine-verbindung-pro-host) nennt den Ort des Sockets, die Keepalive-Einstellungen und die Prüfung, die Sie vor einem breiten Lauf ausführen sollten.

## Become

`become` führt Tasks über `sudo` oder `su` als anderer Benutzer aus.

```yaml
- hosts: web
  become: true
  tasks:
    - name: Install nginx
      apt:
        name: nginx
        state: present

    - name: Check the app as its own user
      command: /opt/app/bin/healthcheck
      become_user: app
```

### So funktioniert es

Volant verpackt nicht jeden Task in `sudo`. Es startet einen zweiten Agent für den Zielbenutzer:

```sh
sudo -H <form> -u <user> -- volant-agent
```

`<form>` ist `-n` oder `-k -S -p ''`, je nach einer Probe, die weiter unten beschrieben wird. Volant fügt `-H` nur hinzu, wenn kein `become_flags` gesetzt ist: Eine Flag-Liste ersetzt es. Tasks ohne Rechteausweitung nutzen weiter den unprivilegierten Agent. Der Agent selbst weiß nichts von der Rechteausweitung. [Entscheidungsdokument 0004](/de/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) erklärt, warum.

Der Agent mit erweiterten Rechten bleibt bis zum Ende des Plays bestehen, sodass ein Play mit solchen Tasks `sudo` pro Host nur einmal bezahlt.

### Welche Einstellung gewinnt

`become`, `become_user` und `become_method` sind an einem Play, einem Block und einem Task erlaubt. Gemessen an Ansible:

1. Die Variablen `ansible_become` und `ansible_become_user` eines Hosts schlagen beide Keywords, in beide Richtungen.
2. Das Keyword am Task schlägt das Keyword am Play.
3. Die Standardwerte aus der `ansible.cfg` sowie `-b`, `--become-user` und `--become-method` gelten zuletzt.

`become_method` ist `sudo` oder `su`. Jede andere Methode wird noch nicht unterstützt, ebenso die vollständig qualifizierten Schreibweisen `ansible.builtin.sudo` und `ansible.builtin.su`. Volant hält an, bevor ein Task läuft, und nennt die Methode, statt die Rechte über einen Mechanismus auszuweiten, den Sie nicht verlangt haben. Stammt die Methode eines Laufs mit Rechteausweitung aus `--become-method`, der `ansible.cfg` oder `ansible_become_method`, endet der Lauf vor jedem Task mit Exit-Code 2.

`become_user` kann ein Template wie `{{ app_user }}` sein. Eines, das sich je Schleifenelement ändert, wird abgelehnt, weil ein Batch die Rechte nur auf einen Benutzer ausweitet.

`become_exe` und `become_flags` werden aus dem Keyword und aus `ansible_become_exe` und `ansible_become_flags` gelesen, wobei die Hostvariable gewinnt. Beide werden so in Wörter zerlegt, wie eine Shell es tut, und ein nicht geschlossenes Anführungszeichen lässt den Task fehlschlagen.

### Das Become-Passwort

Das Passwort kommt aus einer dieser Quellen:

- `-K` (`--ask-become-pass`), das auf Unix die Terminalanzeige der Eingabe abschaltet.
- `--become-password-file`, das die erste Zeile einer Datei liest.
- `ansible_become_password`, `ansible_become_pass` oder das methodeneigene `ansible_sudo_pass` oder `ansible_su_pass`, das die beiden anderen übertrumpft. [Hostvariablen](#hostvariablen) nennt, welche Variable gewinnt.

Es wird einmal in die Standardeingabe von `sudo` geschrieben. Es erscheint nie in einer Kommandozeile, einem Log oder einem Ergebnis: Die Typen, die es tragen, geben in der Debug-Ausgabe `<redacted>` aus, und die eigene Fehlerausgabe von `sudo` wird davon bereinigt, bevor sie eine Meldung erreicht.

### Die sudo-Probe

Volant fragt `sudo`, welche Form zu verwenden ist, statt eine anzunehmen. Es probiert zuerst `sudo -n` und weicht auf `sudo -k -S -p ''` nur aus, wenn `sudo` selbst die Anfrage wegen fehlender Authentifizierung ablehnt. [Entscheidungsdokument 0004](/de/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) erklärt, warum ein Raten die Verbindung blockieren würde. Auf einem Host, der schon einmal lief, startet Volant zuerst den gecachten Agent unter `sudo -n` in derselben `ssh`-Sitzung wie seine Prüfungen und führt die Probe nur aus, wenn das nicht klappt, wie [Der Agent auf Ihren Hosts](/de/docs/internals/agent/) beschreibt.

Schlägt die Rechteausweitung fehl, sei es wegen eines fehlenden oder falschen Passworts oder einer `sudoers`-Regel, die den Befehl verbietet, schlägt der Task fehl, und der Host bleibt erreichbar. Die Meldungen sind die von Ansible: `Missing sudo password` oder `Incorrect sudo password`.

### su

`su` hat kein Flag, um ein Passwort ohne Terminal zu lesen, daher beantwortet Volant stattdessen seine Eingabeaufforderung. Das Passwort wird einmal an die Eingabeaufforderung geschrieben und nie in eine Kommandozeile, ein Log oder die Ausgabe von `-vvv` gesetzt. Ein falsches Passwort lässt den Task mit `Incorrect su password` fehlschlagen. Kommt die Eingabeaufforderung nie, endet der Task mit `Timeout (12s) waiting for privilege escalation prompt`, wie in ansible-core.

Über eine `local`-Verbindung läuft `su` auf einem eigenen Terminal. Ein falsches Passwort lässt den Task mit `Premature end of stream waiting for become success.` und dem, was `su` ausgegeben hat, fehlschlagen, wie ansible-core es dort tut. Das Warten auf die Eingabeaufforderung dauert bei einer lokalen Verbindung immer 12 Sekunden, was auch `-T` sagt.

## Noch nicht vorhanden

- Windows-Hosts und Netzwerkgeräte.
- Die Verbindungs-Plugins außer `ssh` und `local`, aufgelistet in der [Unterstützungsmatrix](/de/docs/reference/parity/#collection-connection).
- `doas`, `pbrun` und die anderen become-Methoden. Sie lassen sich als weitere Wege ergänzen, den Agent zu starten, ohne das Protokoll zu ändern.

[Keywords](/de/docs/reference/keywords/) ist die vollständige Liste, aus den Tabellen erzeugt, die die Engine liest.
