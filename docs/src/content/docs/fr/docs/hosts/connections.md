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

Toute autre connexion est refusée. Cela couvre `paramiko_ssh`, `winrm`, `psrp`, les plugins de conteneurs et de cloud comme `docker`, `podman`, `kubectl` et `aws_ssm`, ainsi que les écritures qualifiées des deux types pris en charge : `ansible.builtin.ssh`, `ansible.builtin.local` et `ansible.legacy.*`. Toute valeur qui contient un point est refusée, y compris le plugin d'une collection comme `community.docker.docker`. L'hôte est signalé injoignable avec `connection '<name>' is not supported`, et le reste de l'exécution continue.

Un nom simple qu'aucun plugin ne porte fait échouer la tâche avec `the connection plugin '<name>' was not found`, comme dans Ansible. Il en va de même pour `docker` et les autres noms qu'une collection a repris à ansible-core, parce que la collection n'est pas là. Cela vaut que la valeur soit la connexion de l'hôte lui-même, celle d'une cible de `delegate_to`, ou celle de l'hôte qui exécute une tâche `run_once`.

Un mot de passe vient de `-k`, de `--connection-password-file` ou de la variable `ansible_password`. Volant le transmet à `ssh` par `SSH_ASKPASS` et un fichier temporaire privé supprimé à la fin de l'exécution, ce qui demande OpenSSH 8.4 ou plus récent. `ssh` s'exécute alors avec `BatchMode=no` et `NumberOfPasswordPrompts=1` : une seule mauvaise réponse met fin à la tentative. Le mot de passe n'apparaît jamais dans une ligne de commande, dans l'environnement d'un programme ni dans la sortie, `-vvvv` compris. Un `ansible_password` chiffré avec vault fonctionne comme toute variable chiffrée. Sans mot de passe, `ssh` s'exécute avec `BatchMode=yes` et ne pose jamais de question : un hôte qui demanderait un mot de passe ou une phrase secrète est signalé injoignable au lieu de bloquer l'exécution.

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
- Les plugins de connexion nommés sous Types de connexion.
- `doas`, `pbrun` et les autres méthodes become. Elles pourront être ajoutées comme autres façons de démarrer l'agent, sans changer le protocole.

[Mots-clés](/fr/docs/reference/keywords/) est la liste complète, générée à partir des tables que lit le moteur.
