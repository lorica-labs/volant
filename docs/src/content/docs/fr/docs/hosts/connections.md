---
sourceHash: 8ebece6e812f
title: Connexions et become
description: Comment Volant atteint un hôte, les variables d'hôte et les paramètres d'ansible.cfg qu'il lit, et comment become fonctionne avec sudo et su.
---

Cette page répond à ces questions : comment Volant atteint vos hôtes, quelles variables contrôlent la connexion, et comment exécuter des tâches sous un autre utilisateur avec `become`.

Volant atteint un hôte géré, y dépose un petit agent statique (un programme qui exécute vos tâches et rend compte), puis lui parle sur la même connexion jusqu'à la fin de l'exécution.

## Types de connexion

`ansible_connection` choisit le transport. Sa valeur par défaut est `ssh`, comme dans Ansible.

| Valeur | Ce qu'elle fait |
|---|---|
| `ssh` | Lance le client OpenSSH de votre `PATH`. Volant construit lui-même la ligne de commande, sans enveloppe et sans bibliothèque SSH en Rust : votre `~/.ssh/config` s'applique donc. |
| `local` | Lance l'agent comme processus fils sur le contrôleur, sans réseau. |
| `docker` | Lance l'agent dans un conteneur en marche via `docker exec -i`, comme `community.docker.docker`. Voir [Conteneurs Docker](#conteneurs-docker). |
| `podman` | Lance l'agent dans un conteneur Podman en marche via `podman exec -i`, rootless ou non, comme `containers.podman.podman`. Voir [Conteneurs Podman](#conteneurs-podman). |

Tout autre nom est résolu par l'ansible-core du contrôleur, comme `ansible-playbook` le résout, une fois par nom et par exécution. `ansible.builtin.ssh` et `ansible.builtin.local` se résolvent vers les deux plugins ci-dessus et s'exécutent. Un nom court qu'une collection a repris à ansible-core est routé vers cette collection quand elle est installée :

| Nom court | Se résout en | Collection dans le paquet `ansible` 12.3.0 |
|---|---|---|
| `docker` | `community.docker.docker` | community.docker 4.8.5 |
| `podman` | `containers.podman.podman` | containers.podman 1.18.0 |
| `buildah` | `containers.podman.buildah` | containers.podman 1.18.0 |
| `kubectl` | `kubernetes.core.kubectl` | kubernetes.core 6.2.0 |
| `lxd` | `community.general.lxd` | community.general 11.4.2 |
| `chroot` | `community.general.chroot` | community.general 11.4.2 |
| `aws_ssm` | `community.aws.aws_ssm`, que `community.aws` 10.0.0 renvoie vers `amazon.aws.aws_ssm` | amazon.aws 10.1.2 |
| `incus` | pas de nom court : écrivez `community.general.incus` | community.general 11.4.2 |

Avec `-vvv`, un nom routé affiche `redirecting (type: connection) ansible.builtin.chroot to community.general.chroot` sous la première tâche qui l'utilise sur un hôte, une ligne par étape.

Un nom qui se résout vers un plugin que Volant ne pilote pas encore, comme `winrm`, `psrp` ou `kubernetes.core.kubectl`, rend l'hôte injoignable avec `connection '<full name>' is not supported`, et le reste de l'exécution continue. La [matrice de prise en charge](/fr/docs/reference/parity/#collection-connection) liste tous les plugins de connexion des collections du paquet.

Un nom qui ne se résout vers rien fait échouer la tâche avec `the connection plugin '<name>' was not found`, comme dans Ansible. Cela comprend un nom court routé vers une collection qui n'est pas installée, comme `docker` sans community.docker, après l'avertissement qu'ansible-core affiche pour lui, et `paramiko`, qu'ansible-core 2.19.12 ne porte pas sous ce nom. Cela vaut que la valeur soit la connexion de l'hôte lui-même, celle d'une cible de `delegate_to`, ou celle de l'hôte qui exécute une tâche `run_once`.

Un mot de passe vient de `-k`, de `--connection-password-file` ou de la variable `ansible_password`. Volant le transmet à `ssh` par `SSH_ASKPASS` et un fichier temporaire privé supprimé à la fin de l'exécution, ce qui demande OpenSSH 8.4 ou plus récent. `ssh` s'exécute alors avec `BatchMode=no` et `NumberOfPasswordPrompts=1` : une seule mauvaise réponse met fin à la tentative. Le mot de passe n'apparaît jamais dans une ligne de commande, dans l'environnement d'un programme ni dans la sortie, `-vvvv` compris. Un `ansible_password` chiffré avec vault fonctionne comme toute variable chiffrée. Sans mot de passe, `ssh` s'exécute avec `BatchMode=yes` et ne pose jamais de question : un hôte qui demanderait un mot de passe ou une phrase secrète est signalé injoignable au lieu de bloquer l'exécution.

## Conteneurs Docker

`ansible_connection: docker`, ou `community.docker.docker`, exécute un play dans un conteneur en marche, sans sshd dedans ni chemin réseau jusqu'à lui. Les deux noms demandent community.docker installé sur le contrôleur, comme pour `ansible-playbook`. Volant lance le programme `docker` de votre `PATH`, si bien que `DOCKER_HOST`, `DOCKER_CONTEXT` et `DOCKER_CONFIG` l'atteignent par l'environnement.

L'agent monte par `docker exec -i`, donc l'image doit avoir `/bin/sh` et les outils habituels : `uname`, `id`, `ls`, `mkdir`, `df`, `awk`, `head`, `wc`, `chmod`, `mv` et `find`. Une image Alpine ou Debian minimale les a. Une image sans shell, comme distroless, reçoit l'agent par `docker cp` à la place : Volant demande à `docker inspect` l'architecture de l'image, copie l'agent dans le `/tmp` du conteneur, et l'agent se place lui-même dans son répertoire de cache, avec les mêmes contrôles. Le conteneur doit avoir un `/tmp`, `remote_tmp` doit être absolu ou commencer par `~` pour un utilisateur qui a un répertoire personnel, et `become` est refusé, faute de shell pour lancer `sudo`. La copie dans `/tmp` reste quand l'utilisateur du conteneur ne peut pas la supprimer.

| Variable | Rôle |
|---|---|
| `ansible_docker_host` | Le nom ou l'identifiant du conteneur. Elle l'emporte sur `ansible_host`, qui l'emporte sur le nom d'inventaire. |
| `ansible_docker_user` | L'utilisateur sous lequel les commandes s'exécutent, `docker exec -u`. Elle l'emporte sur `ansible_user`, le mot-clé `remote_user` et `-u`. Non définie, l'utilisateur du conteneur lui-même. |
| `ansible_docker_extra_args` | Des arguments placés après `docker` et avant `exec`, découpés sur les espaces simples et non comme des mots de shell. La valeur par défaut vient de `[docker_connection] extra_cli_args`. |
| `ansible_docker_extra_env` | Un dictionnaire de variables posées avec `docker exec -e`. Chaque valeur doit être du texte : un nombre ou un booléen sans guillemets est refusé. |
| `ansible_docker_privileged` | `docker exec --privileged`. La valeur par défaut vient de `ANSIBLE_DOCKER_PRIVILEGED` ou de `[docker_connection] privileged`. |
| `ansible_docker_working_dir` | `docker exec -w`. La valeur par défaut vient de `ANSIBLE_DOCKER_WORKING_DIR` ou de `[docker_connection] working_dir`. |
| `ansible_docker_timeout` | Les secondes que `docker` peut mettre à répondre à la première commande. La valeur par défaut vient de `ANSIBLE_DOCKER_TIMEOUT`, puis de `[docker_connection] timeout`, puis du délai de connexion. |

`become` avec `sudo` passe par le `sudo` du conteneur. `su` est refusé, parce que `docker exec` ne lui donne pas de terminal. Un conteneur qui n'existe pas ou qui est arrêté, ou un utilisateur que l'image n'a pas, rend l'hôte injoignable avec le message de docker lui-même. ansible-core signale ces trois cas comme `Failed to create temporary directory` : ce message diffère donc volontairement.

Avec `-vvvv`, les lignes affichent `ESTABLISH DOCKER CONNECTION FOR USER: <user>`, puis une ligne `EXEC [...]` par commande, qui sont les commandes propres à Volant. Quand le play ne nomme pas d'utilisateur, il vient de `docker inspect` : `root` quand l'image n'en nomme aucun, `?` quand docker ne peut pas le dire.

## Conteneurs Podman

`ansible_connection: podman`, ou `containers.podman.podman`, exécute un play dans un conteneur Podman, rootless ou non, sans démon ni ssh. Les deux noms demandent containers.podman installé sur le contrôleur. L'agent monte comme pour docker : par `podman exec -i` quand l'image a un shell, par `podman cp` quand elle n'en a pas.

Volant lit les options comme les lit le code source du plugin, qui diffère de sa documentation. Le conteneur est `ansible_host`, sinon le nom d'inventaire ; le `ansible_podman_host` documenté n'est pas lu. L'utilisateur est `ansible_user`, le mot-clé `remote_user` ou `-u`, passé en `podman exec --user`.

| Variable | Rôle |
|---|---|
| `ansible_podman_executable` | Le programme `podman`, un nom cherché dans votre `PATH` ou un chemin. La valeur par défaut vient de `ANSIBLE_PODMAN_EXECUTABLE`. |
| `ansible_podman_extra_args` | Des arguments placés après `podman` et avant la sous-commande, découpés comme le ferait un shell. La valeur par défaut vient de `ANSIBLE_PODMAN_EXTRA_ARGS` ou de `[defaults] podman_extra_args`. |

Le plugin lance `podman exec` sans `-i`, il ne peut donc pas passer son stdin à une commande. L'agent en a besoin, alors Volant lance `podman exec -i` ; c'est le seul mot que ses lignes de commande ajoutent. Volant ne lance jamais `podman mount`. `become` avec `sudo` passe par le `sudo` du conteneur, et `su` est refusé. Avec `-vvvv`, chaque commande s'affiche en ligne `RUN [...]`, comme le plugin l'écrit, sans ligne `ESTABLISH`.

## Variables d'hôte

Volant lit celles-ci par hôte, depuis l'inventaire ou depuis toute autre source de variables. Un test compare cette liste aux noms que lit le code de connexion : elle reste donc synchronisée avec le moteur.

| Variable | Effet |
| --- | --- |
| `ansible_connection` | `ssh` (par défaut) ou `local` |
| `ansible_host`, `ansible_ssh_host` | Adresse à laquelle se connecter. Par défaut, le nom dans l'inventaire. Une valeur qui commence par `-` ou contient une espace ou un caractère de contrôle est refusée avant le lancement de `ssh`. |
| `ansible_port`, `ansible_ssh_port` | `ssh -p`. Un nombre entre guillemets fonctionne comme un nombre nu. |
| `ansible_user`, `ansible_ssh_user` | `ssh -l` |
| `ansible_password`, `ansible_ssh_pass`, `ansible_ssh_password` | Le mot de passe de connexion, proposé à `ssh` comme décrit plus haut. |
| `ansible_private_key_file`, `ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_timeout`, `ansible_ssh_timeout` | Secondes pendant lesquelles `ssh` attend la connexion, `ssh -o ConnectTimeout`. |
| `ansible_ssh_executable` | Le programme `ssh` à lancer. Par défaut, le `ssh` de votre `PATH`. |
| `ansible_ssh_common_args` | Arguments `ssh` supplémentaires, découpés comme le ferait un shell |
| `ansible_ssh_extra_args` | Les mêmes, ajoutés après les arguments communs |
| `ansible_ssh_use_tty` | Exécute les commandes `raw` et `script` sur un terminal, comme `ssh -tt`. Actif sauf si la valeur est false. La valeur par défaut vient de [`usetty`](/fr/docs/reference/configuration/#ssh-connection-usetty). |
| `ansible_remote_tmp` | Répertoire qui contient le cache de l'agent sur cet hôte |
| `ansible_become` | Élever les privilèges ou non, en remplaçant le mot-clé `become` |
| `ansible_become_user`, `ansible_sudo_user`, `ansible_su_user` | Utilisateur cible de l'élévation |
| `ansible_become_method` | `sudo` ou `su` |
| `ansible_become_password`, `ansible_become_pass`, `ansible_sudo_pass`, `ansible_su_pass` | Mot de passe de l'élévation |
| `ansible_become_exe`, `ansible_sudo_exe`, `ansible_su_exe` | Le programme d'élévation |
| `ansible_become_flags`, `ansible_sudo_flags`, `ansible_su_flags` | Ses options |
| `ansible_su_prompt_l10n` | Une liste d'invites que `su` peut afficher. Elle remplace celles qui sont intégrées. |

Quand un hôte définit une variable et un de ses alias, celui qu'Ansible fait gagner s'applique. Le tableau les liste du plus fort au plus faible, avec le mot-clé qui règle la même chose :

| Réglage | Variables | Mot-clé |
|---|---|---|
| Adresse | `ansible_ssh_host`, `ansible_host` | |
| Port | `ansible_ssh_port`, `ansible_port` | `port` |
| Utilisateur | `ansible_ssh_user`, `ansible_user` | `remote_user` |
| Fichier de clé | `ansible_ssh_private_key_file`, `ansible_private_key_file` | |
| Délai | `ansible_ssh_timeout`, `ansible_timeout` | |
| Mot de passe | `ansible_ssh_password`, `ansible_ssh_pass`, `ansible_password` | |
| Connexion | `ansible_connection` | `connection` |
| Utilisateur, programme et options de l'élévation | `ansible_sudo_*` ou `ansible_su_*` (ceux de la méthode), puis `ansible_become_*` | `become_user`, `become_exe`, `become_flags` |
| Mot de passe de l'élévation | `ansible_sudo_pass` ou `ansible_su_pass`, puis `ansible_become_pass`, puis `ansible_become_password` | |

Une variable de mot de passe l'emporte sur `-k` et `--connection-password-file`, et une variable réglée sur une chaîne vide ne propose aucun mot de passe. Une variable de connexion fonctionne à tous les niveaux qui définissent des variables : inventaire, `group_vars`, `vars` d'un play, bloc, `vars` d'une tâche et `-e`.

Les mots-clés fonctionnent sur un play, un bloc et une tâche, et `-c` définit la connexion des hôtes qui n'en nomment aucune. L'ordre est celui d'Ansible : une variable d'hôte comme `ansible_user` l'emporte, puis la tâche, le bloc et le play, puis la ligne de commande et `ansible.cfg`. Les facts qu'un hôte rapporte sur lui-même ne choisissent jamais son utilisateur, son port ni sa connexion. Un mot-clé écrit sous forme de modèle est rendu une fois par tâche. Ces valeurs font échouer la tâche : un nom d'utilisateur qui commence par `-` ou contient une espace, un guillemet ou un caractère de contrôle, un port hors de 1-65535, et un nom de connexion qui contient autre chose que des lettres, des chiffres, `_` et `.`. Une tâche déléguée utilise le `remote_user` et le `port` de la tâche, sauf si l'hôte délégué définit son propre `ansible_user` ou `ansible_port`.

Un guillemet non fermé dans `ansible_ssh_common_args` ou `ansible_ssh_extra_args` fait échouer l'hôte au lieu d'être ignoré. Cela compte derrière un bastion : abandonner silencieusement un `ProxyCommand` ouvrirait une connexion directe vers une adresse qui peut appartenir à une autre machine.

Les paramètres d'`ansible.cfg` qui touchent les connexions, comme `timeout`, `remote_user`, `host_key_checking` et `forks`, sont listés dans [Configuration](/fr/docs/reference/configuration/).

## Le cache de l'agent

L'agent est un petit programme statique, d'environ 1.2 MB pour Linux x86_64. Volant le téléverse une fois par hôte et le réutilise à chaque exécution suivante : un hôte ne contient donc que l'agent et, quand des modules Python ont tourné, un cache de charges utiles.

Il se trouve dans `<remote_tmp>/volant-agent-<version>-<uid>/volant-agent`, donc, avec le `remote_tmp` par défaut, sous `~/.ansible/tmp/` dans un répertoire personnel. La version dans le chemin est celle du contrôleur, ce qui évite à un contrôleur mis à jour de lancer un ancien agent. Une tâche qui élève ses privilèges exécute son propre agent sous l'identité du `become_user`, mis en cache dans le répertoire personnel de cet utilisateur : un hôte porte donc un cache par compte sous lequel l'exécution agit.

Quand des modules Python ont tourné, un second répertoire, `volant-blobs-<version>-<uid>`, se trouve à côté du répertoire de l'agent dans le même `remote_tmp` et contient les charges utiles de modules que Volant a envoyées.

La façon dont l'agent arrive dépend de la cible. Par `ssh`, et dans un conteneur qui a un shell, l'agent est passé à un script shell sur la connexion, qui contrôle le répertoire et écrit le fichier. Dans un conteneur sans shell, la commande de copie du runtime y dépose l'agent, et l'agent se place lui-même avec les mêmes contrôles ; docker le fait avec `docker cp`.

Pour effacer les deux, supprimez les deux répertoires. Rien d'autre ne reste :

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-agent-* ~/.ansible/tmp/volant-blobs-*'
```

[L'agent sur vos hôtes](/fr/docs/internals/agent/#le-cache-de-lagent) couvre le téléversement, la réparation d'un cache cassé et les contrôles de permissions.

## Connexions persistantes

Un hôte garde sa connexion à l'agent pendant toute l'exécution, d'un play à l'autre. Les tâches circulent sur cette connexion unique au lieu d'en ouvrir une nouvelle à chaque fois. Si la connexion meurt entre deux plays, Volant se reconnecte une fois, et un second échec signale l'hôte injoignable.

Certaines tâches ajoutent une connexion :

- L'élévation de privilèges. Un hôte qui utilise `become` détient une seconde connexion, vers un agent qui tourne sous l'utilisateur cible. Elle est gardée jusqu'à la fin du play : un play de tâches élevées démarre donc un seul agent élevé, pas un par tâche. Un hôte ne détient qu'une connexion élevée à la fois : passer à un autre utilisateur la remplace.
- La délégation. Une tâche déléguée s'exécute sur la connexion propre à l'hôte délégué, avec les réglages de ce dernier. Un hôte dont le play délègue à trois autres hôtes détient ces trois connexions en plus de la sienne.

## Une connexion par hôte

Tous les lancements de `ssh` que fait Volant pour un hôte de l'inventaire partagent une seule connexion OpenSSH : seul le premier paie l'échange de clés et l'authentification. La connexion partagée reste ouverte 30 secondes après sa dernière session. OpenSSH fait le travail : `~/.ssh/config`, `Match`, `Include`, `ProxyJump` et votre ssh-agent s'appliquent exactement comme pour `ansible-playbook`.

Volant vous laisse gérer le partage quand vos propres arguments `ssh` ou votre configuration définissent déjà `ControlMaster`, `ControlPath` ou `ControlPersist` pour l'hôte. Réglez `[volant] ssh_control_master = false` ou `VOLANT_SSH_CONTROL_MASTER=0` pour le désactiver, voir [Configuration](/fr/docs/reference/configuration/#volant-ssh-control-master).

Chaque connexion coûte trois descripteurs de fichier au contrôleur. Une limite stricte de 1024 fichiers ouverts arrête une exécution à environ 330 connexions, ou à environ 169 hôtes qui élèvent leurs privilèges. [L'agent sur vos hôtes](/fr/docs/internals/agent/#une-connexion-par-hôte) donne l'emplacement du socket, les réglages de keepalive et la vérification à faire avant une exécution large.

## Become

`become` exécute des tâches sous un autre utilisateur par `sudo` ou `su`.

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

### Fonctionnement

Volant n'enveloppe pas chaque tâche dans `sudo`. Il démarre un second agent pour l'utilisateur cible :

```sh
sudo -H <form> -u <user> -- volant-agent
```

`<form>` est `-n` ou `-k -S -p ''`, selon une sonde décrite plus bas. Volant ajoute `-H` seulement quand aucun `become_flags` n'est défini : une liste d'options le remplace. Les tâches qui n'élèvent pas leurs privilèges continuent d'utiliser l'agent non privilégié. L'agent lui-même ne sait rien de l'élévation. La [fiche de décision 0004](/fr/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explique pourquoi.

L'agent élevé est gardé jusqu'à la fin du play : un play de tâches élevées paie donc `sudo` une fois par hôte.

### Quel réglage l'emporte

`become`, `become_user` et `become_method` sont acceptés sur un play, un bloc et une tâche. Mesuré par rapport à Ansible :

1. Les variables d'hôte `ansible_become` et `ansible_become_user` l'emportent sur les deux mots-clés, dans les deux sens.
2. Le mot-clé de la tâche l'emporte sur celui du play.
3. Les valeurs par défaut d'`ansible.cfg` et `-b`, `--become-user` et `--become-method` s'appliquent en dernier.

`become_method` vaut `sudo` ou `su`. Toute autre méthode n'est pas encore prise en charge, de même que les écritures qualifiées `ansible.builtin.sudo` et `ansible.builtin.su`. Volant s'arrête avant l'exécution d'une tâche et nomme la méthode, plutôt que d'élever les privilèges par un mécanisme que vous n'avez pas demandé. Quand une exécution élève ses privilèges et que la méthode vient de `--become-method`, d'`ansible.cfg` ou de `ansible_become_method`, l'exécution se termine avec le code 2 avant toute tâche.

`become_user` peut être un modèle comme `{{ app_user }}`. Un modèle qui change à chaque élément de boucle est refusé, parce qu'un lot s'élève vers un seul utilisateur.

`become_exe` et `become_flags` sont lus depuis le mot-clé et depuis `ansible_become_exe` et `ansible_become_flags`, la variable d'hôte l'emportant. Les deux sont découpés en mots comme le fait un shell, et un guillemet non fermé fait échouer la tâche.

### Le mot de passe become

Le mot de passe vient de l'une de ces sources :

- `-K` (`--ask-become-pass`), qui désactive l'écho du terminal sous Unix.
- `--become-password-file`, qui lit la première ligne d'un fichier.
- `ansible_become_password`, `ansible_become_pass`, ou `ansible_sudo_pass` ou `ansible_su_pass` propres à la méthode, qui l'emportent sur les deux autres. [Variables d'hôte](#variables-dhôte) liste la variable qui gagne.

Il est écrit une seule fois sur l'entrée standard de `sudo`. Il n'apparaît jamais dans une ligne de commande, un journal ou un résultat : les types qui le portent affichent `<redacted>` dans la sortie de débogage, et la sortie d'erreur de `sudo` en est expurgée avant d'atteindre un message.

### La sonde sudo

Volant demande à `sudo` quelle forme employer au lieu d'en supposer une. Il essaie d'abord `sudo -n`, et ne se rabat sur `sudo -k -S -p ''` que lorsque `sudo` lui-même rejette la demande faute d'authentification. La [fiche de décision 0004](/fr/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explique pourquoi une supposition bloquerait le lien. Sur un hôte qui a déjà servi, Volant démarre d'abord l'agent en cache sous `sudo -n` dans la même session `ssh` que ses vérifications, et ne lance la sonde que si cela échoue, comme le décrit [L'agent sur vos hôtes](/fr/docs/internals/agent/).

Quand l'élévation échoue, faute de mot de passe, avec un mauvais mot de passe ou à cause d'une règle `sudoers` qui interdit la commande, la tâche échoue et l'hôte reste joignable. Les messages sont ceux d'Ansible : `Missing sudo password` ou `Incorrect sudo password`.

### su

`su` n'a pas d'option pour lire un mot de passe sans terminal : Volant répond donc à son invite. Le mot de passe est écrit une seule fois dans l'invite, jamais placé dans une ligne de commande, un journal ni la sortie de `-vvv`. Un mauvais mot de passe fait échouer la tâche avec `Incorrect su password`. Une invite qui ne vient jamais met fin à la tâche avec `Timeout (12s) waiting for privilege escalation prompt`, comme dans ansible-core.

Sur une connexion `local`, `su` s'exécute sur un terminal à part. Un mauvais mot de passe fait échouer la tâche avec `Premature end of stream waiting for become success.` et ce que `su` a affiché, comme le fait ansible-core dans ce cas. L'attente de l'invite est toujours de 12 secondes sur une connexion locale, quelle que soit la valeur de `-T`.

## Pas encore disponible

- Les hôtes Windows et les équipements réseau.
- Les plugins de connexion autres que `ssh` et `local`, listés dans la [matrice de prise en charge](/fr/docs/reference/parity/#collection-connection).
- `doas`, `pbrun` et les autres méthodes become. Elles pourront être ajoutées comme autres façons de démarrer l'agent, sans changer le protocole.

[Mots-clés](/fr/docs/reference/keywords/) est la liste complète, générée à partir des tables que lit le moteur.
