---
sourceHash: 053a11b38c33
title: Rôles, includes et imports
description: Où Volant cherche les rôles, comment fonctionnent les dépendances, les spécifications d'arguments et les variables de rôle, et comment les imports et les includes amènent d'autres fichiers dans un play.
---

Cette page répond à ces questions : où Volant trouve un rôle, comment se comportent ses dépendances et ses variables, et comment `import_*` et `include_*` font entrer d'autres fichiers. Volant lit le répertoire d'un rôle pendant la compilation du play et insère ses tâches dans la liste des étapes : rien dans un rôle n'attend donc un hôte.

## Trouver un rôle

Les entrées de `roles:`, les dépendances de `meta/main.yml` et `import_role` nomment tous un rôle de la même façon. Volant cherche dans cet ordre :

1. `<playbook_dir>/roles`
2. `roles_path` d'`ansible.cfg`, ou `ANSIBLE_ROLES_PATH`. Sans l'un ni l'autre, les valeurs par défaut sont `~/.ansible/roles`, `/usr/share/ansible/roles` et `/etc/ansible/roles`.
3. `<playbook_dir>` lui-même

Une entrée relative de `roles_path` est lue par rapport au répertoire d'`ansible.cfg`, ou par rapport au répertoire de travail quand elle vient de `ANSIBLE_ROLES_PATH`. Chaque entrée est rendue absolue, `.` et `..` retirés, avant d'être utilisée. L'`ansible_search_path` d'un rôle et les lignes `included:` montrent ce chemin, comme le fait ansible-core.

Un nom en trois parties comme `acme.demo.hello` est aussi cherché sous `ansible_collections/acme/demo/roles/hello`, d'abord dans un répertoire `collections/` à côté du playbook, puis dans chaque entrée de `collections_path`.

Un rôle que personne ne trouve arrête l'exécution avec le code de sortie 1, et le message liste tous les chemins essayés.

## Dans un rôle

`defaults/` et `vars/` sont lus fichier d'abord, puis répertoire : `vars/main.yml` s'il existe, sinon tous les fichiers de `vars/main/`, dans l'ordre des noms. `tasks/`, `handlers/` et `meta/` prennent un seul fichier et jamais un répertoire. Sur une entrée de rôle, `tasks_from`, `vars_from`, `defaults_from` et `handlers_from` choisissent un autre fichier que `main`.

Le nom est un chemin sous le répertoire correspondant, avec ou sans son extension (`install`, `install.yml`, `sub/install`). Un nom qui sort de ce répertoire, comme `../../../x` ou un chemin absolu hors de lui, arrête l'exécution avec `Failed loading '...' for role (name) as it is not inside the expected role path`. Volant suit un lien symbolique situé dans le répertoire, où qu'il pointe, comme le fait ansible-core.

Le répertoire `handlers/` d'un rôle est lu avec ses tâches. Lors d'un flush, les handlers du rôle s'exécutent avant ceux du play.

## Dépendances

Les `dependencies` de `meta/main.yml` sont des rôles à part entière. Elles s'exécutent avant le rôle qui les a demandées, et une dépendance que deux rôles partagent s'exécute une seule fois. `allow_duplicates: true` désactive cette déduplication pour un rôle. Cela vaut aussi d'une instruction à l'autre : un rôle que le `roles:` du play a exécuté, ou qu'un `include_role` précédent a amené pour un hôte, n'est pas exécuté de nouveau comme dépendance d'un rôle qu'un `import_role` ou un `include_role` amène plus tard.

La déduplication regarde plus que le nom. Les paramètres de l'entrée, ses `vars:`, son `when:` et ses sélections `*_from` font tous partie de son identité :

```yaml
roles:
  - base                    # runs once...
  - base                    # ...because this is the same entry
  - { role: base, p: v }    # runs again: different parameters
```

## Spécifications d'arguments

`meta/argument_specs.yml` devient une tâche à part. Elle s'exécute après les dépendances du rôle et avant la première tâche du rôle, et elle vérifie les arguments du rôle par rapport à l'entrée de spécification qui porte le nom de son fichier de tâches.

La tâche porte la `short_description` du rôle après un tiret : `--list-tasks` affiche donc `Validating arguments against arg spec 'main' - The spec role`. Elle est taguée `always`, si bien qu'aucune sélection `--tags` ne l'écarte.

Les vérifications s'exécutent dans l'ordre où Ansible les exécute : un argument obligatoire manquant, puis un type, puis une valeur hors de `choices`, puis un argument que la spécification ne nomme pas. Les sous-options, `aliases`, `default` et `mutually_exclusive` ne sont pas encore vérifiés.

## Variables de rôle

Un rôle apporte trois couches de variables, qui se placent à des hauteurs différentes dans l'[ordre de priorité](/fr/docs/variables/precedence/) :

| Couche | Où elle se place |
|---|---|
| `defaults/main.yml` | Tout en bas, sous les variables propres de l'inventaire. |
| `vars/main.yml` | Au-dessus des `vars:` du play, sous celles d'une tâche. |
| Paramètres de rôle, les clés libres d'une entrée comme `{ role: base, p: v }` | Au-dessus des facts. Un paramètre de rôle l'emporte sur un `set_fact` du même nom. |

:::tip
Les `vars:` écrites sur une entrée de rôle ne sont pas un paramètre de rôle. Elles voyagent comme un mot-clé de tâche ordinaire. Les deux écritures se ressemblent : vérifiez laquelle vous avez utilisée quand un rôle lit une valeur que vous n'attendiez pas.
:::

Les `defaults` et `vars` d'un rôle sont visibles de tout le play : `pre_tasks` et les rôles suivants peuvent donc les lire. Pour les étapes propres au rôle, ses valeurs sont remises par-dessus. Les paramètres de rôle restent dans le rôle et dans les rôles dont il dépend.

Une dépendance lit aussi ce que portent les rôles au-dessus d'elle. Ses `vars` se placent au-dessus du `vars/main.yml` de ses parents et de celui de ses propres dépendances, qui l'emporte sur celui des parents. Ses `defaults` vont dans l'autre sens : ceux de ses dépendances, puis ceux de ses parents, puis les siens. Ses paramètres se placent au-dessus de ceux de ses parents : une dépendance sans paramètre de ce nom lit celui du parent le plus proche.

Un `import_role` avec `public: false` les garde pour lui, et un `include_role` fait de même sauf s'il dit `public: true`. Voir [Options de rôle](#options-de-rôle). [`private_role_vars`](/fr/docs/reference/configuration/#defaults-private-role-vars) change ces valeurs par défaut.

Un rôle qu'un `include_role` amène depuis un autre rôle lit les paramètres de ce rôle, au-dessus des `vars:` de l'instruction, ainsi que ses `vars` et ses `defaults`, quel que soit le nombre d'includes entre les deux.

## Imports et includes

Cinq instructions font venir du travail depuis un autre fichier. Elles se rangent en deux familles :

| Instruction | Type | Quand Volant lit le fichier | Ce qu'une liste affiche |
|---|---|---|---|
| `import_tasks` | statique | pendant la compilation du play | chaque tâche à l'intérieur |
| `import_role` | statique | pendant la compilation du play | chaque tâche à l'intérieur |
| `import_playbook` | statique | pendant la compilation du play | chaque play à l'intérieur |
| `include_tasks` | dynamique | quand un hôte atteint l'instruction | l'instruction seule |
| `include_role` | dynamique | quand un hôte atteint l'instruction | l'instruction seule |

Un include dynamique est résolu hôte par hôte, parce que le nom de son fichier peut dépendre des variables de cet hôte. Les nouvelles étapes sont insérées juste derrière l'instruction pendant que le play s'exécute.

### Imports statiques

Un chemin d'`import_tasks` est cherché d'abord à côté du fichier qui importe, puis dans le répertoire `tasks/` du rôle lui-même.

Un chemin d'`import_tasks` peut contenir un template. Il est rendu pendant la compilation du play, avec `role_path`, `playbook_dir`, les `vars:` du play, les variables `-e` et les `defaults` et `vars` des rôles : `{{ role_path }}/tests/main.yml` fonctionne donc. Il n'y a pas encore d'hôte, si bien qu'une variable d'inventaire y est indéfinie et arrête le chargement avec le code 4.

`import_playbook` insère les plays de l'autre fichier à l'endroit de l'instruction, et chaque play importé garde le répertoire du fichier d'où il vient. Le nom du fichier est rendu avec les variables `-e` et par le [chemin Python des filtres et lookups](/fr/docs/variables/templating/#filtres-tests-et-lookups-qui-nécessitent-python) : `{{ which }}.yml` fonctionne donc avec `-e which=sub.yml`, de même qu'un nom construit avec `lookup('env', ...)` ou un filtre de collection. Rien n'a encore d'inventaire, donc un nom ne peut pas dépendre d'un hôte. Un nom qui lit une variable non définie arrête le chargement avec le code 4 : `Error processing keyword 'import_playbook': 'nosuchvar' is undefined`.

Trois mots-clés de l'entrée `import_playbook` atteignent tous les plays du fichier :

- `tags` s'ajoutent aux tags de chaque play importé : `--tags` et `--skip-tags` sélectionnent donc ses tâches et les listes les montrent.
- `vars` s'ajoutent aux variables de chaque play importé et l'emportent sur celles du play.
- `when` est placé devant les conditions de chaque tâche, rôle et bloc de chaque play importé, et devant sa collecte de facts. Il n'est pas évalué une seule fois pour tout l'import. Une condition fausse ouvre quand même chaque play et affiche quand même un saut pour chaque tâche et chaque hôte. Les handlers ne sont pas conditionnels.

Les autres mots-clés qu'ansible-core 2.19 accepte sur l'entrée, comme `ignore_errors`, `become` et `environment`, n'ont aucun effet ici, et Volant les ignore de la même façon. Toute autre clé est refusée. Un `tags` ou un `when` qui contient un modèle est refusé aussi, parce que Volant n'a pas de variables quand il charge le playbook.

### Includes dynamiques

De ce que dit l'instruction elle-même, seules ses `vars:` passent dans le fichier inclus. Ces variables entrent au niveau des paramètres de rôle de l'[ordre de priorité](/fr/docs/variables/precedence/), au-dessus des facts et sous `--extra-vars`, et elles s'accumulent à travers les includes imbriqués. Une valeur qui lit un nom pas encore défini descend telle qu'elle est écrite et est rendue quand une tâche en dessous la lit, comme dans Ansible. Les `tags` de l'instruction elle-même s'arrêtent à l'instruction, et son `when` est évalué pour l'instruction seule.

Ce que disent les couches autour de l'instruction atteint bien les tâches incluses : le play, l'entrée de rôle qui a amené le fichier, et les blocs autour. Leurs `tags`, `become`, `become_user`, `no_log`, `when`, `environment`, `delegate_to`, `ignore_errors`, `notify` et `vars` s'appliquent à chaque tâche incluse, et la valeur propre à une tâche l'emporte sur une valeur héritée. C'est la règle d'ansible-core 2.19, qui saute un parent chargé dynamiquement et interroge le grand-parent. Un `check_mode: true` hérité est refusé dans le fichier inclus au lieu d'être abandonné.

Une tâche incluse atterrit dans le bloc et la section où l'instruction a été écrite : un include en échec dans le corps d'un bloc est donc rattrapé par le `rescue` de ce bloc.

Volant vérifie ce qu'un include apporte avant de l'exécuter. À ce moment-là, les hôtes sont connectés et les tâches précédentes ont tourné : un module ou un mot-clé que cette version ne prend pas encore en charge ne peut donc plus arrêter toute l'exécution. Il fait échouer l'instruction d'include pour l'hôte qui l'a atteinte, rien dans le fichier ne s'exécute, et un `rescue` autour de l'instruction peut l'attraper.

Avant la première connexion, Volant collecte les modules Python nommés par chaque fichier de tâches et de handlers des rôles du play. Un module nommé seulement dans `setup-{{ ansible_os_family }}.yml` est donc déjà sur l'hôte quand l'include s'exécute. Un fichier sous `tasks/` ou `handlers/` d'un rôle qui ne s'analyse pas arrête l'exécution avant toute connexion, pour `roles:` et `import_role`. Pour un `include_role`, ce balayage anticipé ne lit pas le rôle : les plays et tâches précédents s'exécutent, puis l'exécution se termine avec le code 4 après la bannière de l'instruction, et un `when: false` sur l'include cache l'échec. Voir [Le chemin Python préchauffé](/fr/docs/internals/python/#comment-ça-marche).

Un `notify` dans un fichier inclus qui ne nomme aucun handler met fin à l'exécution avec le code de sortie 1 et la même phrase que la vérification préalable, comme le fait ansible-core. La vérification préalable est le balayage que Volant lance avant de se connecter à un hôte. Aucun `ignore_errors` ni `rescue` ne peut l'attraper. Voir [Blocs et handlers](/fr/docs/playbooks/blocks/#différences-avec-ansible).

#### Appliquer des mots-clés à ce qu'un include apporte

`apply:` sur `include_tasks` et `include_role` pose des mots-clés sur chaque tâche que l'instruction apporte, et sur les handlers d'un rôle inclus. Il accepte les mots-clés d'un bloc : `tags`, `when`, `vars`, `become`, `environment`, `ignore_errors`, `delegate_to` et le reste. Les imports refusent `apply`, puisqu'ils transmettent déjà les mots-clés que vous écrivez sur eux.

```yaml
- include_tasks:
    file: install.yml
    apply:
      tags: [install]
      become: true
  tags: [always]
```

Les tags de `apply` sont sur les tâches incluses et non sur l'instruction. Avec `--tags install`, l'instruction ne s'exécute que si elle porte un tag que la sélection retient, ici `always`, et alors les tâches incluses s'exécutent. Un `when` dans `apply` est vérifié pour chaque tâche incluse, et un `when` sur l'instruction est vérifié une fois. Les `vars` de `apply` sont des variables de bloc : les `vars:` de l'instruction elle-même l'emportent donc sur elles.

Une clé d'`apply` qu'un bloc n'a pas arrête l'exécution avec le code de sortie 4 quand l'instruction s'exécute, après sa bannière, comme dans ansible-core. Un `apply` qui n'est pas un mapping arrête le chargement.

#### Options de rôle

Avec `public: true`, les `defaults` et `vars` d'un `include_role` sont visibles de tout ce qui s'exécute après l'instruction, et des autres hôtes aussi, parce qu'ansible-core garde la liste des rôles publics dans le play et non dans l'hôte. Une tâche qui s'est exécutée avant l'instruction n'en lit rien, et un include `when: false` n'apporte rien.

Avec `allow_duplicates: false`, l'instruction affiche sa ligne `included:` et n'exécute rien pour un hôte qui s'est déjà vu apporter un rôle identique. Deux instructions sont identiques quand elles nomment le même rôle, les mêmes fichiers et les mêmes `vars:`, et, pour une boucle, le même élément sous la même variable de boucle. `rolespec_validate: false` omet la vérification de la spécification d'arguments. Un `import_role` ou un `include_role` antérieur de ce rôle compte. Un rôle de `roles:` ne compte pas, comme dans Ansible.

Chaque option des quatre instructions, telle que Volant et ansible-core 2.19.12 la lisent :

| Instruction | Option | Valeur par défaut | Volant |
|---|---|---|---|
| `include_role` | `name`, `role` | obligatoire | fonctionne |
| `include_role` | `tasks_from`, `vars_from`, `defaults_from`, `handlers_from` | `main` | fonctionne |
| `include_role` | `public` | `false` | fonctionne |
| `include_role` | `allow_duplicates` | `true` | fonctionne |
| `include_role` | `rolespec_validate` | `true` | fonctionne |
| `include_role` | `apply` | aucune | fonctionne |
| `import_role` | `name`, `role`, les quatre `*_from` | comme ci-dessus | fonctionne |
| `import_role` | `public` | `true` | fonctionne : `false` garde les variables dans le rôle |
| `import_role` | `allow_duplicates` | `true` | fonctionne, décidé pendant la compilation du play |
| `import_role` | `rolespec_validate` | `true` | fonctionne |
| `import_role` | `apply` | aucune | refusé comme dans ansible-core, `Invalid options for import_role: apply` |
| `include_tasks` | `file` ou le nom en forme libre | obligatoire | fonctionne |
| `include_tasks` | `apply` | aucune | fonctionne |
| `import_tasks` | `file` ou le nom en forme libre | obligatoire | fonctionne |
| `import_tasks` | `apply` | aucune | refusé comme dans ansible-core, `Invalid options for import_tasks: apply` |

Une valeur qui n'est pas un booléen dans `public`, `allow_duplicates` ou `rolespec_validate` fait échouer la tâche, ou arrête le chargement pour un `import_role`, avec la phrase d'ansible-core.

### include_vars

`include_vars` produit des variables plutôt que des étapes : il n'est donc ni statique ni dynamique. C'est un module ordinaire côté contrôleur, et ce qu'il lit se place sous `set_fact` et les résultats enregistrés, comme dans Ansible.

| Question | Réponse |
|---|---|
| Que lit-il ? | Un fichier, nommé par `file:` ou écrit comme chemin en forme libre, ou tous les fichiers d'un répertoire, nommé par `dir:`. Les deux formes prennent des options différentes, et donner des options des deux fait échouer la tâche. |
| Où un fichier est-il cherché ? | Dans `vars/` puis dans le répertoire lui-même, pour un rôle puis pour le playbook. Un chemin absolu est utilisé tel quel. |
| Où un `dir:` est-il cherché ? | Il n'est pas cherché. Une tâche d'un rôle le lit sous le `vars/` du rôle, et `dir: vars/x` nomme un répertoire du rôle lui-même quand il existe. Toute autre tâche le lit par rapport au répertoire du fichier qui contient la tâche. |
| Dans quel ordre un répertoire est-il lu ? | Les répertoires triés selon leur chemin complet en tant que texte, puis les fichiers de chaque répertoire triés par nom. Les fichiers et répertoires cachés sont lus, et les liens vers des répertoires sont suivis. |
| Et si deux fichiers définissent la même clé ? | Un fichier plus tardif remplace en entier les clés d'un fichier plus ancien. |
| Que contient le résultat ? | `ansible_facts` et `ansible_included_var_files`, les fichiers dans l'ordre où ils ont été lus. Un fichier vide n'ajoute rien, et il est quand même listé. |
| Et un fichier chiffré avec vault ? | Une tâche qui en lit un est masquée comme une tâche `no_log` : `-v` montre la ligne censurée, et un élément de boucle qui en a lu un affiche `(censored due to no_log)`. |
| Mode check ? | La tâche s'exécute en mode check, comme dans Ansible. |
| Qu'est-ce qui échoue ? | Un répertoire manquant, un chemin qui n'est pas un répertoire, un fichier sans extension valide et un fichier qui n'est pas un mapping font échouer la tâche avec `message` renseigné, comme dans Ansible. Un échec dans un répertoire garde dans le résultat les variables des répertoires qui précèdent celui en échec, mais une tâche en échec ne définit aucune variable. Un fichier qui ne peut pas être analysé ou déchiffré lève l'erreur à la place, et la tâche n'a pas de résultat propre. Une option du mauvais type échoue avec la phrase propre à Python. |

Les options de la forme `dir:` :

| Option | Signification |
|---|---|
| `dir` | Le répertoire à lire. |
| `depth` | Combien de niveaux lire, le répertoire étant le premier. `0`, la valeur par défaut, les lit tous. |
| `files_matching` | Une expression régulière recherchée dans chaque nom de fichier, pas dans son chemin. Les fichiers qui ne correspondent pas sont écartés. |
| `ignore_files` | Une liste d'expressions régulières, ou une chaîne unique qui en contient, séparées par des espaces. Chacune est ancrée à la fin du nom. Les fichiers qui correspondent sont écartés. |
| `extensions` | Les extensions qu'un fichier peut avoir, sans le point. Par défaut `yaml`, `yml` et `json`. La comparaison est sensible à la casse. |
| `ignore_unknown_extensions` | Écarter un fichier qui a une autre extension. Sans elle, un tel fichier fait échouer la tâche. |
| `name` | Mettre tout sous cette seule variable. Fonctionne aussi pour un fichier unique. |
| `hash_behaviour` | `merge` pose chaque variable sur la variable de même nom que la tâche voit déjà, les mappings fusionnés jusqu'au bout. Toute autre valeur remplace. |

### Limite d'imbrication

Une instruction imbriquée sur 32 niveaux est rejetée. Un seul compteur couvre l'imbrication de rôles, `import_role` dans les tâches d'un rôle et `import_tasks` dans un fichier importé, parce qu'un cycle peut passer par n'importe quel mélange des trois.

## Différences avec Ansible


- Un nom de rôle contenant un `.` est essayé comme un chemin avant de l'être comme un rôle de collection : un répertoire de rôle dont le nom contient un point reste donc accessible.
- `collections_path` d'`ansible.cfg` et `ANSIBLE_COLLECTIONS_PATH` sont lus comme `roles_path`. Ce comportement n'a pas été mesuré par rapport à ansible-core. Le répertoire `collections/` à côté du playbook l'a été.
- Un fichier `import_tasks` manquant est signalé avec les deux premières phrases d'ansible-core mais pas la troisième, qui nomme un numéro d'erreur Python pour un module qui n'a jamais été appelé.
- `import_playbook` écrit comme une tâche est rejeté avant le début du play au lieu d'échouer sous une bannière de play. Le code de sortie est le même, 2.
- Un fichier qui s'importe lui-même est rejeté avec le code de sortie 1 et un message propre. Ansible récurse jusqu'à ce que l'interpréteur abandonne, au code de sortie 250 avec une longue trace d'appels.
- Une vérification de spécification d'arguments en échec a `"failed": true` dans son résultat, comme tout échec côté contrôleur dans Volant.


- Un fichier qui s'inclut lui-même atteint cette limite et est signalé comme un échec de tâche avec le code de sortie 2. Ansible récurse jusqu'à épuisement de la pile Python, au code de sortie 250.
- Un fichier inclus hors de `tasks/` et `handlers/` d'un rôle qui ne s'analyse pas fait échouer la tâche, avec le code de sortie 2 et un récapitulatif. Ansible se termine avec le code 4 et sans récapitulatif. Un échec que le récapitulatif comptabilise est plus utile, et un `rescue` peut l'attraper.
- Un include en échec dans une boucle affiche la ligne habituelle `failed: [host] (item=...)`. Ansible n'affiche qu'un `[ERROR]` sur la sortie d'erreur standard.
- `allow_duplicates: false` sur un `import_role` compare les instructions pendant la compilation du play : un `when` sur l'instruction précédente ne change donc pas le résultat pour un hôte.
