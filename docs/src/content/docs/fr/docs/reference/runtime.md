---
sourceHash: 86490c3c05d2
title: Le runtime géré
description: Le Python et l'ansible-core que Volant installe pour lui-même, où ils se trouvent, ce qu'ils téléchargent, et comment les gérer.
---

Volant a besoin d'un Python 3 avec `ansible-core` sur le contrôleur pour les modules Python, la collecte de facts et les filtres et lookups qu'il n'implémente pas lui-même. Si la machine n'en a pas, Volant installe le sien, une seule fois, et l'utilise ensuite. Vous n'avez pas besoin d'avoir Ansible installé pour exécuter un playbook.

Le runtime est CPython 3.13 et ansible-core 2.19.12, la version à laquelle Volant s'aligne. Il vit dans un seul répertoire que Volant possède : il ne touche donc aucun Python du système ni aucun de vos environnements virtuels.

## Quand il est installé

Une exécution installe le runtime quand elle a besoin de l'assistant Python et ne trouve aucun interpréteur utilisable. Elle affiche une ligne sur la sortie d'erreur et continue :

```text
Installing Volant's Python runtime (cpython-3.13.16-ansible-core-2.19.12-98a5d8b2) ...
```

Une exécution qui ne fait que collecter des facts n'installe jamais rien. Vous pouvez aussi installer le runtime à l'avance, par exemple dans une image :

```sh
volant runtime install
```

Sur une machine vierge avec une connexion normale, l'installation a pris environ 8 secondes. Les exécutions suivantes le trouvent avec deux vérifications de fichiers.

## Quel interpréteur une exécution utilise

Le premier de ceux-ci qui s'applique :

1. `VOLANT_PYTHON`, quand il est défini. Volant utilise exactement cet interpréteur et n'installe jamais rien.
2. Le runtime géré, quand il est installé.
3. L'environnement virtuel actif, puis `python3` dans votre `PATH`, quand ils ont ansible-core.
4. Un nouveau runtime géré, installé à l'instant.

Volant démarre l'interpréteur avec `-P` : un fichier du répertoire de travail, comme `ansible.py` ou `json.py`, n'est donc jamais importé à la place du vrai module. `-P` demande Python 3.11 ou plus récent, ce dont ansible-core 2.19 a aussi besoin. Un interpréteur de votre cru garde `PYTHONPATH` et son site utilisateur. [`volant doc`](/fr/docs/reference/cli/doc/) choisit lui aussi son interpréteur selon cette règle, et y lance le propre `ansible-doc` d'ansible-core.

`volant runtime status` montre le choix que ferait la prochaine exécution, avec chaque candidat et son origine. Le [paramètre `runtime`](/fr/docs/reference/configuration/#volant) change cet ordre.

## Ce qu'il télécharge

Volant récupère une build CPython autonome et les wheels d'ansible-core avec `curl`, ou `wget` quand `curl` manque. Il lance aussi `tar`. Les URL et le SHA-256 de chaque fichier sont fixés dans le binaire de Volant : chaque version sait donc exactement quels fichiers elle peut utiliser.

- Chaque téléchargement est vérifié par rapport à son empreinte fixée avant que quoi que ce soit n'en soit exécuté. En cas de différence, Volant supprime le téléchargement et s'arrête en montrant les deux empreintes.
- `pip` n'installe que les wheels fixées, avec `--require-hashes` et sans index.
- L'installation se fait dans un répertoire temporaire à côté du répertoire final et est déplacée en place après une vérification que `import ansible` fonctionne. Une installation qui échoue supprime son répertoire temporaire. Une installation interrompue peut laisser un répertoire `.<id>.partial-*`, que l'installation suivante supprime.
- Un fichier de verrou empêche deux processus Volant d'installer en même temps. Le second attend, puis trouve le runtime installé.
- Chaque processus fils s'exécute avec `PYTHONNOUSERSITE=1` et sans `PYTHONPATH` : les paquets de votre répertoire personnel ou de votre environnement ne peuvent donc pas l'atteindre.

Les plateformes sont Linux et macOS en x86_64 et en arm64. Sous Linux, Volant lit la libc de la machine sur laquelle il tourne, glibc ou musl, et choisit la build correspondante, quelle que soit celle pour laquelle il a été compilé. Volant n'a pas été testé sous macOS : aucun Mac n'était disponible. Tout autre système est refusé avec un message qui le nomme et renvoie à `VOLANT_PYTHON`.

## Où il se trouve

Sous `$XDG_DATA_HOME/volant/runtime`, ou `~/.local/share/volant/runtime`. Sous macOS, la valeur par défaut est `~/Library/Application Support/volant/runtime`. Chaque runtime a son propre répertoire, nommé par son identifiant (version de Python, version d'ansible-core et un court hachage de la liste de fichiers fixée) : une version avec d'autres empreintes s'installe donc à côté de l'ancienne. Un runtime occupe environ 155 MB.

## Collections

Volant n'installe pas de collection pendant une exécution. Pour en installer une dans le runtime :

```sh
volant runtime install --collections community.general ansible.posix:2.2.2
volant runtime install --collections requirements.yml
```

`--collections` prend des noms, chacun avec un `:version` facultatif, ou un fichier de dépendances qui se termine par `.yml` ou `.yaml`. Les collections vont dans `collections/` à l'intérieur du runtime. Volant y cherche après les chemins de `ANSIBLE_COLLECTIONS_PATH` et de `[defaults] collections_path` : les collections que vous avez installées vous-même l'emportent donc. [`volant galaxy`](/fr/docs/reference/cli/galaxy/) installe dans la première entrée de ce chemin, `~/.ansible/collections` par défaut, et liste le répertoire du runtime en dernier.

Le runtime contient ansible-core et ses dépendances, rien d'autre. Une collection dont les plugins ont besoin d'un autre paquet Python, comme `jmespath` pour `json_query`, n'y fonctionne pas. Pour celles-là, utilisez votre propre environnement avec `VOLANT_PYTHON`.

## Commandes

| Commande | Ce qu'elle fait |
|---|---|
| `volant runtime install` | Installe le runtime s'il manque. |
| `volant runtime install --collections NAME_OR_FILE...` | Installe aussi des collections. |
| `volant runtime install --from FILE` | Installe depuis un fichier fait par `pack`, sans téléchargement. |
| `volant runtime install --from FILE --sha256 HASH` | Installe depuis le fichier seulement si son SHA-256 est `HASH`. |
| `volant runtime pack FILE` | Écrit le runtime installé dans un fichier et affiche son SHA-256. |
| `volant runtime status` | Montre le mode, l'interpréteur que la prochaine exécution utilise, et le runtime géré. |
| `volant runtime path` | Affiche le Python du runtime géré. Sort avec le code 1 quand il n'est pas installé. |
| `volant runtime remove [ID]` | Supprime le runtime actuel, ou celui qui est nommé. |
| `volant runtime remove --all` | Supprime tous les runtimes. |

### Machines sans réseau

Empaquetez un runtime sur une machine qui a un réseau, copiez le fichier, et installez-le sur une machine de la même plateforme :

```sh
volant runtime pack runtime.tar.gz
# packed runtime.tar.gz
# sha256 3f1c...e9
volant runtime install --from runtime.tar.gz --sha256 3f1c...e9
```

`install --from` refuse un fichier fait pour une autre plateforme ou un autre runtime, et nomme les deux. `pack` affiche le SHA-256 du fichier sur une ligne `sha256`. Avec `--sha256 HASH`, `install --from` refuse un fichier dont l'empreinte diffère, avant de le décompresser, et montre les deux empreintes. Sans lui, l'installation affiche l'empreinte du fichier qu'elle a utilisé, pour que vous puissiez la comparer vous-même. Installer un paquet exécute le Python qu'il contient : passez donc l'empreinte que vous avez obtenue de la machine qui l'a fait.

## Le désactiver

Réglez `runtime = system` dans la section `[volant]` d'`ansible.cfg`, ou `VOLANT_RUNTIME=system`. Volant n'utilise alors que ce qui existait avant, et un interpréteur manquant est refusé avec l'ancien message plus une indication sur le runtime. Avec `managed`, Volant n'utilise que `VOLANT_PYTHON` ou le runtime géré, installe le runtime quand il manque, et ignore `python3` dans le `PATH`. Voir [Configuration](/fr/docs/reference/configuration/#volant).
