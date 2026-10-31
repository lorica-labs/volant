---
sourceHash: 864ee37f7fcb
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
| `podman` | Startet den Agent über `podman exec -i` in einem laufenden Podman-Container, rootless oder nicht, wie `containers.podman.podman`. Siehe [Podman-Container](#podman-container). |
| `chroot` | Startet den Agent über `chroot` in einem Verzeichnisbaum auf dem Controller, wie `community.general.chroot`. Siehe [Chroot-Verzeichnisse](#chroot-verzeichnisse). |
| `buildah` | Startet den Agent über `buildah run` in einem Buildah-Arbeitscontainer, wie `containers.podman.buildah`. Siehe [Buildah-Arbeitscontainer](#buildah-arbeitscontainer). |
| `lxd` | Startet den Agent über `lxc exec` in einer LXD-Instanz, wie `community.general.lxd`; `community.general.incus` tut dasselbe für Incus. Siehe [LXD- und Incus-Instanzen](#lxd--und-incus-instanzen). |
| `kubectl` | Startet den Agent über `kubectl exec` in einem Container eines Kubernetes-Pods, wie `kubernetes.core.kubectl`. Siehe [Kubernetes-Pods](#kubernetes-pods). |
| `aws_ssm` | Startet den Agent über eine AWS-Systems-Manager-Sitzung auf einer EC2-Instanz, wie `amazon.aws.aws_ssm`. Siehe [AWS-Systems-Manager-Sitzungen](#aws-systems-manager-sitzungen). |
| `paramiko_ssh` | Startet den OpenSSH-Client anstelle der veralteten paramiko-Verbindung von ansible-core; `paramiko` ist dasselbe. Siehe [Paramiko](#paramiko). |

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

Ein Name, der sich zu einem Plugin auflöst, das Volant nicht selbst steuert, etwa `winrm`, `psrp`, `community.general.jail` oder ein Plugin in einem Verzeichnis `connection_plugins/` neben dem Playbook, nimmt den zweiten Weg: siehe [Verbindungen, die nur ansible-core hat](#verbindungen-die-nur-ansible-core-hat). Ohne das ansible-core des Controllers, das gefragt werden kann, macht ein solcher Name den Host nicht erreichbar, mit `connection '<full name>' is not supported`, und der übrige Lauf geht weiter. Die [Unterstützungsmatrix](/de/docs/reference/parity/#collection-connection) listet jedes Verbindungs-Plugin der Collections im Paket auf.

Ein Name, der sich zu nichts auflöst, lässt den Task mit `the connection plugin '<name>' was not found` fehlschlagen, wie bei Ansible. Dazu gehört ein kurzer Name, der zu einer nicht installierten Collection geleitet wird, etwa `docker` ohne community.docker, nach der Warnung, die ansible-core dafür ausgibt, und `ansible.builtin.paramiko`, das ansible-core 2.19.12 unter diesem Namen nicht mitbringt. Das gilt unabhängig davon, ob der Wert die eigene Verbindung des Hosts ist, die eines `delegate_to`-Ziels oder die des Hosts, der einen `run_once`-Task ausführt.

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

## Podman-Container

`ansible_connection: podman` oder `containers.podman.podman` führt ein Play in einem Podman-Container aus, rootless oder nicht, ohne Daemon und ohne ssh. Beide Namen brauchen containers.podman auf dem Controller. Der Agent kommt wie bei docker hinein: über `podman exec -i`, wenn das Image eine Shell hat, über `podman cp`, wenn nicht.

Volant liest die Optionen so, wie der Quellcode des Plugins sie liest, und das weicht von seiner Dokumentation ab. Der Container ist `ansible_host`, sonst der Inventory-Name; das dokumentierte `ansible_podman_host` wird nicht gelesen. Der Benutzer ist `ansible_user`, das Schlüsselwort `remote_user` oder `-u`, übergeben als `podman exec --user`.

| Variable | Wirkung |
|---|---|
| `ansible_podman_executable` | Das Programm `podman`, ein Name, der in Ihrem `PATH` gesucht wird, oder ein Pfad. Der Standard kommt aus `ANSIBLE_PODMAN_EXECUTABLE`. |
| `ansible_podman_extra_args` | Argumente nach `podman` und vor dem Unterbefehl, getrennt wie von einer Shell. Der Standard kommt aus `ANSIBLE_PODMAN_EXTRA_ARGS` oder `[defaults] podman_extra_args`. |

Das Plugin startet `podman exec` ohne `-i` und kann einem Befehl daher kein stdin übergeben. Der Agent braucht eines, also startet Volant `podman exec -i`; das ist das einzige Wort, das seine Befehlszeilen hinzufügen. Volant startet nie `podman mount`. `become` mit `sudo` läuft über das `sudo` des Containers, und `su` wird abgelehnt. Mit `-vvvv` erscheint jeder Befehl als `RUN [...]`-Zeile, wie das Plugin sie schreibt, ohne `ESTABLISH`-Zeile.

## Chroot-Verzeichnisse

`ansible_connection: chroot` oder `community.general.chroot` führt ein Play in einem Verzeichnisbaum auf dem Controller aus: ein Image-Build, ein `debootstrap`-Baum, eine Rettungsaktion. Der Inventory-Name oder `ansible_host` ist das Verzeichnis. Beide Namen brauchen community.general auf dem Controller. Der Lauf braucht root, weil `chroot` es braucht.

| Variable | Wirkung |
|---|---|
| `ansible_chroot_exe` | Das Programm `chroot`, ein Name, der in Ihrem `PATH` gesucht wird, oder ein absoluter Pfad. Der Standard kommt aus `ANSIBLE_CHROOT_EXE` oder `[chroot_connection] exe`. |
| `ansible_chroot_disable_root_check` | Überspringt die Prüfung, ob der Controller als root läuft. Der Standard kommt aus `ANSIBLE_CHROOT_DISABLE_ROOT_CHECK` oder `[chroot_connection] disable_root_check`. |
| `ansible_executable` | Die Shell, unter der die Befehle laufen, standardmäßig `/bin/sh`. Der Standard kommt aus `ANSIBLE_EXECUTABLE` oder `[defaults] executable`. |

Bevor irgendetwas läuft, schlägt der Task fehl, wie unter ansible-core, bei einem Pfad, der kein Verzeichnis ist, bei einem Baum ohne `bin/sh` und bei einem Controller, der nicht root ist, außer die Prüfung ist abgeschaltet. Der Agent kommt über `chroot <dir> /bin/sh` hinein, das Skript, das ihn schreibt, läuft also im Baum, und kein Pfad, den es schreibt, kann den Baum verlassen, welche symbolischen Links er auch enthält. Kann `df` nicht sagen, wie viel Platz frei ist, etwa in einem Baum ohne eingehängtes `/proc`, entfällt die Platzprüfung. `become` funktioniert, wenn der Baum `sudo` hat. Mit `-vvvv` zeigen die Zeilen `THIS IS A LOCAL CHROOT DIR`, dann eine `EXEC [...]`-Zeile pro Befehl.

## Buildah-Arbeitscontainer

`ansible_connection: buildah` oder `containers.podman.buildah` konfiguriert den Arbeitscontainer, den `buildah from <image>` anlegt, sodass ein Play ein Image Schicht für Schicht baut, das `buildah commit` dann speichert. Beide Namen brauchen containers.podman auf dem Controller. Wie bei podman ist der Container `ansible_host`, sonst der Inventory-Name, und der Benutzer ist `ansible_user`, das Schlüsselwort `remote_user` oder `-u`; `root` fügt keine Option hinzu. Das Programm ist das `buildah` aus Ihrem `PATH`, und die `BUILDAH_*`-Variablen erreichen es über die Umgebung.

Der Agent startet mit `buildah run [--user U] <container> -- <agent>`. Volant startet nie `buildah mount`, das für einen rootless Benutzer `buildah unshare` braucht: Ein Arbeitscontainer ohne Shell bekommt den Agent über `buildah copy`. Rootless buildah schreibt bei jedem Befehl Warnungen über Capabilities, deshalb wird das stderr des Agents nicht angezeigt. Mit `-vvvv` erscheint jeder Befehl als `RUN [...]`-Zeile, ohne `ESTABLISH`-Zeile.

## LXD- und Incus-Instanzen

`ansible_connection: lxd` oder `community.general.lxd` führt ein Play in einer LXD-Instanz aus, lokal oder auf einem entfernten Server, über den Befehl `lxc`. `community.general.incus` tut dasselbe für Incus mit dem Befehl `incus`; es hat in ansible-core keinen kurzen Namen. Beide brauchen community.general auf dem Controller.

| Variable | Wirkung |
|---|---|
| `ansible_lxd_host`, `ansible_incus_host` | Die Instanz, bis zum ersten Punkt: `web.example.com` ist `web`. Sie hat Vorrang vor `ansible_host`, das Vorrang vor dem Inventory-Namen hat. |
| `ansible_lxd_remote`, `ansible_incus_remote` | Der entfernte Server, standardmäßig `local`. |
| `ansible_lxd_project`, `ansible_incus_project` | Das Projekt. Incus übergibt immer eines, standardmäßig `default`. |
| `ansible_lxd_executable`, `ansible_incus_executable` | Die Shell, unter der die Befehle laufen. Sie hat Vorrang vor `ansible_executable`; standardmäßig `/bin/sh`. |

Der Benutzer ist `ansible_user`, das Schlüsselwort `remote_user` oder `-u`, standardmäßig `root`. Für jeden anderen Benutzer läuft jeder Befehl über die become-Methode, `/bin/su <user> -c`, außer `lxd_become_method` oder `incus_become_method` nennt eine andere. Eine Instanz ohne Shell bekommt den Agent über `lxc file push` oder `incus file push`, wenn der Benutzer root ist. Eine fehlende oder angehaltene Instanz macht den Host nicht erreichbar, mit den Worten des Plugins, etwa `instance not running: web`; Incus fügt den entfernten Server und das Projekt hinzu. Mit `-vvvv` zeigen die Zeilen `ESTABLISH LXD CONNECTION FOR USER: <user>` (`Incus` für Incus), die `INFO:`-Zeile des Plugins für einen Benutzer außer root, dann eine `EXEC [...]`-Zeile pro Befehl.

## Kubernetes-Pods

`ansible_connection: kubectl` oder `kubernetes.core.kubectl` führt ein Play über `kubectl exec -i` in einem Container eines Pods aus. Beide Namen brauchen kubernetes.core auf dem Controller und `kubectl` in Ihrem `PATH`.

| Variable | Wirkung |
|---|---|
| `ansible_kubectl_pod` | Der Pod. Ohne sie ist der Pod `ansible_host`, sonst der Inventory-Name. |
| `ansible_kubectl_container` | Der Container, für einen Pod mit mehreren. |
| `ansible_kubectl_namespace` | Der Namespace. |
| `ansible_kubectl_kubeconfig`, `ansible_kubectl_config` | Die kubeconfig-Datei. Eine als Mapping angegebene kubeconfig landet in einer privaten temporären Datei, die am Ende des Laufs gelöscht wird. |
| `ansible_kubectl_extra_args` | Weitere Argumente für `kubectl`, an Leerzeichen getrennt. |
| `ansible_kubectl_local_env_vars` | Ein Mapping von Variablen, die in die Umgebung jedes `kubectl`-Befehls gesetzt werden. |
| `ansible_kubectl_context` | Der Kontext in der kubeconfig. |
| `ansible_kubectl_server`, `ansible_kubectl_host` | Der API-Server. |
| `ansible_kubectl_username`, `ansible_kubectl_user`, `ansible_kubectl_password` | Benutzername und Passwort für den API-Server. |
| `ansible_kubectl_token`, `ansible_kubectl_api_key` | Ein Bearer-Token für den API-Server. |
| `ansible_kubectl_client_cert`, `ansible_kubectl_cert_file`, `ansible_kubectl_client_key`, `ansible_kubectl_key_file` | Das Client-Zertifikat und sein Schlüssel. |
| `ansible_kubectl_ca_cert`, `ansible_kubectl_ssl_ca_cert` | Die Zertifizierungsstelle des API-Servers. |
| `ansible_kubectl_validate_certs`, `ansible_kubectl_verify_ssl` | Wird unverändert nach `--insecure-skip-tls-verify` übergeben, wie beim Plugin: `true` ergibt `True`, und `false` fügt nichts hinzu. |

Jede Option kann auch aus der Umgebungsvariable `K8S_AUTH_*` kommen, die das Plugin liest; eine Hostvariable hat Vorrang. Die Optionen erreichen `kubectl` in der Reihenfolge des Plugins. Befehle laufen als Benutzer des Containers: Das Plugin kann keinen wählen, daher wird `ansible_user` ignoriert. Ein Container ohne Shell kann den Agent nicht aufnehmen, da `kubectl cp` darin `tar` braucht. Ein fehlender Pod, ein fehlender Namespace und ein beendeter Pod machen den Host nicht erreichbar, mit dem, was `kubectl` gemeldet hat. Mit `-vvvv` zeigen die Zeilen `ESTABLISH kubectl CONNECTION`, dann eine `EXEC [...]`-Zeile pro Befehl, in der Passwort und Token als `********` erscheinen.

## AWS-Systems-Manager-Sitzungen

`ansible_connection: amazon.aws.aws_ssm` führt ein Play über AWS Systems Manager Session Manager auf einer EC2-Instanz aus, ohne eingehenden Port, Schlüsselpaar oder Bastion. Der kurze Name `aws_ssm` führt über community.aws dorthin, das daher ebenfalls installiert sein muss. Der Controller braucht `session-manager-plugin` und boto3 in seinem Python, die Instanz den SSM-Agent.

| Variable | Wirkung |
|---|---|
| `ansible_aws_ssm_instance_id` | Die Instanz. Ohne sie ist die Instanz `ansible_host`, sonst der Inventory-Name. |
| `ansible_aws_ssm_region` | Die Region, sonst `AWS_DEFAULT_REGION` oder `AWS_REGION`; standardmäßig `us-east-1`. |
| `ansible_aws_ssm_profile` | Das AWS-Profil, sonst `AWS_PROFILE`. |
| `ansible_aws_ssm_access_key_id`, `ansible_aws_ssm_secret_access_key`, `ansible_aws_ssm_session_token` | Die Zugangsdaten, sonst die gleichnamigen `AWS_*`-Umgebungsvariablen, sonst was boto3 findet. Sie werden nie ausgegeben, auch nicht mit `-vvvv`. |
| `ansible_aws_ssm_plugin` | Das Programm `session-manager-plugin`, sonst `AWS_SESSION_MANAGER_PLUGIN`, `/usr/local/bin/session-manager-plugin` oder das in Ihrem `PATH`. |
| `ansible_aws_ssm_document` | Das SSM-Dokument, das die Sitzung startet; ohne Angabe die Standard-Shell des Kontos. |
| `ansible_aws_ssm_timeout` | Wie viele Sekunden die Sitzung für jeden Schritt des Verbindungsaufbaus hat; standardmäßig 60. |

Befehle laufen als Benutzer der Sitzung, `ssm-user` unter Amazon Linux. `become` wählt einen anderen, und `ansible_user` wird ignoriert, wie beim Plugin. Volant schickt nichts über S3, daher werden die `ansible_aws_ssm_bucket_*`-Variablen, die das Plugin braucht, nicht gelesen. Das Terminal der Sitzung gibt nur gültiges UTF-8 zurück, daher spricht der Agent mit dem Controller in Base64-Zeilen, mit etwa 400 KiB/s in jede Richtung, und der erste Lauf auf einer Instanz lädt den Agent einmal hoch, in etwa zehn Sekunden. Eine Instanz, die der SSM-Agent nicht verwaltet, oder abgewiesene Zugangsdaten machen den Host mit den Worten von boto3 nicht erreichbar, wo das Plugin die Task scheitern lässt. Wenn der Lauf endet oder der Controller beendet wird, schließt sich die Sitzung und der Agent stoppt mit ihr. Mit `-vvvv` zeigen die Zeilen `ESTABLISH SSM CONNECTION TO: <instance>`, dann eine `EXEC: <Befehl>`-Zeile pro Befehl.

## Paramiko

`ansible_connection: paramiko_ssh` ist die Verbindung von ansible-core über die Python-Bibliothek paramiko. ansible-core 2.19.12 markiert sie als veraltet und entfernt sie in 2.21. Volant erreicht diese Hosts stattdessen mit dem Programm `ssh`, paramiko muss also nicht installiert sein. Jede Option des Plugins wird zur nächstliegenden `ssh`-Option, und der erste Host, der sich verbindet, gibt einmal pro Lauf die Warnung von ansible-core aus:

```text
[DEPRECATION WARNING]: The paramiko connection plugin is deprecated. This feature will be removed from ansible-core version 2.21.
```

`deprecation_warnings = False` schaltet sie ab. `paramiko`, `_paramiko_ssh` und `ansible.builtin.paramiko_ssh` sind dasselbe Plugin.

| Variable | Wirkung |
|---|---|
| `ansible_paramiko_host` | Die Adresse. Sie schlägt `ansible_ssh_host`, das `ansible_host` schlägt, danach den Inventarnamen. |
| `ansible_paramiko_port` | `ssh -p`. Schlägt `ansible_ssh_port` und `ansible_port`, danach das Schlüsselwort `port`, `ANSIBLE_REMOTE_PARAMIKO_PORT`, `ANSIBLE_REMOTE_PORT`, `[paramiko_connection] remote_port` und `[defaults] remote_port`. |
| `ansible_paramiko_user` | `ssh -l`. Schlägt `ansible_ssh_user` und `ansible_user`, danach das Schlüsselwort `remote_user`, `ANSIBLE_PARAMIKO_REMOTE_USER`, `[paramiko_connection] remote_user` und die üblichen Einstellungen für den entfernten Benutzer. |
| `ansible_paramiko_private_key_file` | `ssh -i`. Schlägt `ansible_ssh_private_key_file` und `ansible_private_key_file`, danach `ANSIBLE_PARAMIKO_PRIVATE_KEY_FILE`, `[paramiko_connection] private_key_file` und die üblichen Schlüsseleinstellungen. |
| `ansible_paramiko_password`, `ansible_paramiko_pass` | Das Passwort, das `ssh` wie bei einem `ssh`-Host bekommt. Sie schlagen `ansible_ssh_password`, `ansible_ssh_pass` und `ansible_password`. |
| `ansible_paramiko_timeout` | `ConnectTimeout`. Schlägt `ansible_ssh_timeout`, danach `ANSIBLE_PARAMIKO_TIMEOUT`, `ANSIBLE_SSH_TIMEOUT`, `[paramiko_connection] timeout`, `[ssh_connection] timeout` und das Verbindungs-Timeout. `ansible_timeout` wird nicht gelesen, wie beim Plugin. |
| `ansible_paramiko_host_key_checking`, `ansible_ssh_host_key_checking`, `ansible_host_key_checking` | Ob der Host-Schlüssel geprüft wird, in dieser Reihenfolge, danach `ANSIBLE_PARAMIKO_HOST_KEY_CHECKING`, `ANSIBLE_SSH_HOST_KEY_CHECKING`, `ANSIBLE_HOST_KEY_CHECKING`, `[paramiko_connection] host_key_checking` und `[defaults] host_key_checking`. Ausgeschaltet bekommt `ssh` `StrictHostKeyChecking=no` und `UserKnownHostsFile=/dev/null`. |
| `ansible_paramiko_use_rsa_sha2_algorithms` | Ausgeschaltet bekommt `ssh` `HostKeyAlgorithms=+ssh-rsa` und `PubkeyAcceptedAlgorithms=+ssh-rsa`, für einen Server, der RSA-Schlüssel nur als `ssh-rsa` kennt. Bietet ein Server SHA-2-Signaturen an, nimmt `ssh` weiterhin diese. Auch `ANSIBLE_PARAMIKO_USE_RSA_SHA2_ALGORITHMS` und `[paramiko_connection] use_rsa_sha2_algorithms`. |
| `ansible_paramiko_proxy_command` | `ProxyCommand`; `ssh` ersetzt `%h`, `%p` und `%r` wie paramiko. Auch `ANSIBLE_PARAMIKO_PROXY_COMMAND` und `[paramiko_connection] proxy_command`. |

Die Optionen ohne Variable kommen aus `ANSIBLE_PARAMIKO_<NAME>` oder `[paramiko_connection]`:

| Option | Was `ssh` bekommt |
|---|---|
| `host_key_auto_add` | Eingeschaltet, bei eingeschalteter Schlüsselprüfung: `StrictHostKeyChecking=accept-new`. Der Schlüssel eines neuen Hosts wird angenommen, ein geänderter weiterhin abgelehnt. |
| `record_host_keys` | Ausgeschaltet, bei eingeschalteter Schlüsselprüfung: `UserKnownHostsFile=/dev/null ~/.ssh/known_hosts ~/.ssh/known_hosts2`. `ssh` prüft gegen die üblichen Dateien und schreibt einen neuen Schlüssel in die erste, die nichts behält. |
| `look_for_keys` | Ausgeschaltet, mit einer angegebenen Schlüsseldatei: `IdentitiesOnly=yes`, nur dieser Schlüssel wird probiert. Ohne Schlüsseldatei ändert es nichts. |
| `pty` | Ausgeschaltet: kein Terminal, wie `ansible_ssh_use_tty: false` bei `ssh`. |
| `banner_timeout` | Wird gelesen und geprüft, ohne Wirkung: `ssh` kennt keine solche Einstellung. |

Was sich vom Plugin unterscheidet:

- `~/.ssh/config` und `/etc/ssh/ssh_config` werden nicht gelesen, wie bei paramiko: jedes `ssh` bekommt `-F /dev/null`. Auch `ansible_ssh_common_args`, `ansible_ssh_extra_args` und `ansible_ssh_executable` werden nicht gelesen.
- Ein unbekannter Host-Schlüssel macht den Host bei eingeschalteter Prüfung und ausgeschaltetem `host_key_auto_add` mit der Meldung von `ssh` unerreichbar. Das Plugin fragt auf dem Terminal.
- Die Fehlermeldungen sind die von `ssh`, für einen unbekannten Host-Schlüssel wie für eine abgelehnte Anmeldung, und der Host ist in beiden Fällen unerreichbar, wie beim Plugin.
- Bei ausgeschaltetem `look_for_keys` und angegebener Schlüsseldatei werden die Schlüssel von `ssh-agent` auch nicht angeboten. paramiko bietet sie weiterhin an.
- Die Verbindung zu einem Host wird zwischen den Tasks geteilt, wie bei `ssh`.

Mit `-vvvv` lautet die erste Zeile `ESTABLISH PARAMIKO SSH CONNECTION FOR USER: <user> on PORT <port> TO <address>`, danach folgen die `SSH: EXEC`-Zeilen eines `ssh`-Hosts.

## Verbindungen, die nur ansible-core hat

Volant erreicht einen Host auf einem von zwei Wegen. Für `ssh`, `local` und die oben aufgeführten Verbindungen startet es seinen Agent auf dem Host und schickt ihm die Tasks. Für jedes andere Verbindungs-Plugin, das das ansible-core des Controllers laden kann, läuft kein Agent: jeder Task, der den Host braucht, geht an den Python-Helfer von Volant, der das Verbindungs-Plugin lädt, sich verbindet, das Action-Plugin des Tasks darüber ausführt wie `ansible-playbook` und die Verbindung wieder schließt. ansible-core liest jede Option des Plugins aus den Variablen des Hosts, der `ansible.cfg` und der Umgebung, wie immer.

Alles andere am Lauf bleibt gleich: Schleifen, `when`, `register`, `changed_when`, `failed_when`, `until` und `retries`, `ignore_errors`, `no_log`, `serial`, `delegate_to`, Handler, `async` mit `poll: 0` und `async_status`, der Check-Modus und das Recap. Ein Task, der auf dem Controller läuft, etwa `debug` oder `set_fact`, läuft dort wie bisher.

- Ein Plugin, das sich nicht verbinden kann, macht den Host mit der eigenen Meldung des Plugins unerreichbar.
- Die Zeilen, die das Plugin bei `-vvv` ausgibt, etwa `<host> ESTABLISH ... CONNECTION`, erscheinen unter dem Task bei derselben Ausführlichkeit.
- `ansible_password`, die anderen geheimen Variablen des Plugins und das Become-Passwort werden in jeder Zeile und jedem Ergebnis als `********` geschrieben.
- Es laufen nie mehr Helfer-Prozesse gleichzeitig, als `forks` erlaubt.
- `become` nimmt die Eskalationsmethode des Tasks, `sudo` oder `su`.

Jeder Task bezahlt den Aufbau der Verbindung des Plugins, wie bei `ansible-playbook`, daher ist dieser Weg pro Task langsamer als der des Agents.

### Verbindungen, die einen Prozess behalten

`network_cli`, `netconf` und `httpapi` halten ihre Verbindung zum Gerät in einem eigenen Prozess, wie unter `ansible-playbook`. Vor der ersten Task, die das Gerät braucht, startet der Helper den `ansible-connection`-Stub von ansible-core mit dem Python, den Plugin-Pfaden und der Ausführlichkeit des Helpers. Der Stub startet den Prozess, der an einem Socket unter `~/.ansible/pc` lauscht, festgelegt durch `ANSIBLE_PERSISTENT_CONTROL_PATH_DIR` oder `[persistent_connection] control_path_dir`. Der Name des Sockets ist ein Hash aus Adresse, Port, Benutzer und Lauf, daher nutzt jede spätere Task für dasselbe Gerät den Prozess erneut, aus jedem Helper.

- Ein Prozess, der nicht starten kann, lässt die Task mit seinem Fehler scheitern, nicht den Host. `-vvv` fügt seinen Traceback hinzu.
- `meta: reset_connection` beendet den Prozess des Hosts, und das Ende jedes Plays beendet alle Prozesse, die seine Tasks gestartet haben, wie bei `ansible-playbook`. Ein Prozess, der `persistent_connect_timeout` Sekunden (standardmäßig 30) untätig bleibt, endet von selbst, und ein Befehl wartet höchstens `persistent_command_timeout` Sekunden (30).
- Bei `-vvvv` erscheinen die Zeilen, die ansible-core beim Start des Prozesses ausgibt, etwa `<address> attempting to start connection` und `<address> local domain socket path is ...`, unter der Task.
- `persistent_log_messages` protokolliert jeden Austausch mit dem Gerät, Passwörter eingeschlossen, und ansible-core warnt davor mit seinem eigenen Text.

## Netzwerkgeräte

`ansible_connection: network_cli`, `netconf` oder `httpapi`, oder ihre vollen Namen unter `ansible.netcommon`, erreichen einen Switch, einen Router oder eine Firewall über die eigenen Plugins von ansible.netcommon, die ansible-core im Python-Helper von Volant lädt. Der Controller braucht ansible.netcommon und ansible.utils sowie die Collection der Plattform des Geräts, etwa `cisco.ios`. `ansible_network_os` nennt diese Plattform (`cisco.ios.ios`). Ein `network_cli`-Host ohne sie ist mit `Unable to automatically determine host network os` unerreichbar.

Die Module der Netzwerk-Collections, etwa `ansible.netcommon.cli_command` oder `cisco.ios.ios_config`, laufen wie unter `ansible-playbook` auf dem Controller und erreichen das Gerät über den dauerhaften Prozess der Verbindung. Ein Modul, das eine andere Verbindung als die des Hosts braucht, scheitert mit der Meldung seiner Collection, etwa `Connection type local is not valid for this module`.

Jede Verbindung braucht Python-Pakete, die ansible-core nicht mitbringt: paramiko oder ansible-pylibssh für `network_cli`, ncclient für `netconf`. Mit der verwalteten Runtime von Volant installiert `volant runtime install --extras network` sie, und ein Lauf, der sie braucht, hält vor der ersten Verbindung an, solange sie fehlen. Mit `VOLANT_PYTHON` installieren Sie sie in dieser Umgebung.

Die Plugins lesen ihre Optionen selbst aus den Variablen des Hosts, `ansible.cfg` und der Umgebung, wie `ansible-doc -t connection ansible.netcommon.network_cli` sie auflistet.

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

Hosts, die über `local` erreicht werden, teilen sich stattdessen ihre Agents. Jeder dieser Agents läuft auf dem Controller als derselbe Benutzer, also gibt ein Host, der auf die anderen wartet, seinen Agent an den nächsten laufenden Host weiter, und der Lauf behält höchstens `forks` untätige. Ein Play auf 500 lokalen Hosts hält etwa so viele Agents, wie `forks` erlaubt, nicht 500. Ein eskalierter lokaler Agent bleibt bei seinem Host.

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

Schlägt die Rechteausweitung fehl, sei es wegen eines fehlenden oder falschen Passworts oder einer `sudoers`-Regel, die den Befehl verbietet, schlägt der Task fehl, und der Host bleibt erreichbar. Die Meldungen sind die von Ansible: `Missing sudo password` oder `Incorrect sudo password`. Ein Task mit einem anderen Passwort bekommt einen eigenen Agenten, sodass ein falsches Passwort auch dann fehlschlägt, wenn das richtige schon funktioniert hat, und `ignore_errors` diesen Fehler wie jeden anderen abdeckt.

### su

`su` hat kein Flag, um ein Passwort ohne Terminal zu lesen, daher beantwortet Volant stattdessen seine Eingabeaufforderung. Das Passwort wird einmal an die Eingabeaufforderung geschrieben und nie in eine Kommandozeile, ein Log oder die Ausgabe von `-vvv` gesetzt. Ein falsches Passwort lässt den Task mit `Incorrect su password` fehlschlagen. Kommt die Eingabeaufforderung nie, endet der Task mit `Timeout (12s) waiting for privilege escalation prompt`, wie in ansible-core.

Über eine `local`-Verbindung läuft `su` auf einem eigenen Terminal. Ein falsches Passwort lässt den Task mit `Premature end of stream waiting for become success.` und dem, was `su` ausgegeben hat, fehlschlagen, wie ansible-core es dort tut. Das Warten auf die Eingabeaufforderung dauert bei einer lokalen Verbindung immer 12 Sekunden, was auch `-T` sagt.

## Noch nicht vorhanden

- Windows-Hosts.
- Die Verbindungs-Plugins außer `ssh` und `local`, aufgelistet in der [Unterstützungsmatrix](/de/docs/reference/parity/#collection-connection).
- `doas`, `pbrun` und die anderen become-Methoden. Sie lassen sich als weitere Wege ergänzen, den Agent zu starten, ohne das Protokoll zu ändern.

[Keywords](/de/docs/reference/keywords/) ist die vollständige Liste, aus den Tabellen erzeugt, die die Engine liest.
