---
sourceHash: 40f537f587c4
title: Boucles et nouvelles tentatives
description: loop, with_items et loop_control, et comment until, retries et delay répètent une tâche.
---

Cette page répond à deux questions : comment répéter une tâche sur une liste d'éléments avec `loop` et `with_<lookup>`, et comment relancer une tâche jusqu'à ce qu'elle réussisse avec `until`, `retries` et `delay`.

## Boucles

```yaml
- name: Create the service accounts
  command: useradd --system {{ item }}
  loop:
    - app
    - worker
  loop_control:
    label: "{{ item }}"
  register: accounts
```

`loop` et `with_items` ne peuvent pas apparaître ensemble sur la même tâche. `with_items` aplatit un niveau de listes imbriquées et `loop` non, comme dans Ansible. Enregistrer une tâche en boucle collecte une liste `results` avec une entrée par élément.

`loop_control` lit `loop_var`, `label`, `index_var`, `pause`, `extended`, `extended_allitems` et `break_when`. Une sous-clé qu'ansible-core ne connaît pas est rejetée au chargement du playbook. Toutes les formes `with_<lookup>` s'exécutent. Voir [Mots-clés](/fr/docs/reference/keywords/#under-loop_control) et [Boucles `with_`](#boucles-with_).

## Boucles `with_`

`with_<name>` équivaut à `loop: "{{ lookup('<name>', *terms, wantlist=True) }}"`, et il fonctionne pour tous les plugins de lookup qu'ansible-core possède :

| Lookup | Exemples |
|---|---|
| Les vingt-cinq de `ansible.builtin` | `with_dict`, `with_nested`, `with_subelements`, `with_sequence` |
| Les lookups qu'ansible-core redirige vers `community.general` | `with_flattened`, `with_cartesian`, `with_filetree` |
| Le lookup d'une collection, sous son nom complet | `with_community.general.dependent` |
| Un plugin dans un répertoire `lookup_plugins/` à côté du playbook ou d'un rôle | le nom de fichier du plugin, sous la forme `with_<name>` |

Un lookup intégré est aussi accepté sous la forme `with_ansible.builtin.<name>` et `with_ansible.legacy.<name>`.

L'ansible-core du contrôleur dit si un nom est un lookup, avant la première connexion. Un nom `with_` qui n'en est pas fait échouer le playbook avec `'with_xyz' is not a valid attribute for a Task`, comme dans Ansible. Une tâche avec deux boucles échoue avec `duplicate loop in task: <name>`, et une clé `with_` sans valeur avec `you must specify a value when using with_<name>`.

Un lookup qui lève une erreur fait échouer la tâche avec la phrase propre au plugin. Un lookup que Volant exécute lui-même, en Rust (`file`, `unvault`, `vars`, `pipe`, `template` et `first_found`), échoue avec `The lookup plugin '<name>' failed: <reason>` et la chaîne d'ansible-core, aussi dans un `when:` et dans un fichier modèle. `vars` le formule `No variable named '<name>' was found.`, sans partie plugin. Une valeur de boucle qui est un seul modèle lisant une variable non définie échoue avec `'name' is undefined`. Une liste dont un élément lit une variable non définie échoue avec la phrase du moteur pour ce cas, sans la chaîne qu'Ansible affiche.

## Rendu de chaque élément

Une boucle rend les arguments, le `when`, les mots-clés et le libellé d'un élément quand elle l'atteint, et non avant le début de la boucle.

- Un élément qui ne parvient pas à être rendu échoue seul, sous la forme `failed: [h] (item=2)`. Les éléments suivants s'exécutent quand même, et la tâche se termine par `One or more items failed`.
- Un `break_when` qui termine la boucle en premier masque les erreurs de rendu des éléments qui le suivent.
- Une boucle `debug` ou `set_fact` avec un `loop_control.pause` rend chaque élément après la pause, donc `now()` lit l'heure à laquelle l'élément s'exécute, et un `set_fact` en boucle transmet toujours ce qu'il a défini aux éléments suivants.
- Un `loop_control.label` qui lit une variable non définie fait échouer son élément après l'exécution du module.

## `with_first_found` et `with_fileglob`

```yaml
- name: Distribute the matching config
  copy:
    src: "{{ item }}"
    dest: /etc/app/config.yml
  with_first_found:
    - "{{ ansible_distribution }}.yml"
    - default.yml

- name: Copy every image
  copy:
    src: "{{ item }}"
    dest: /var/lib/app/images/
  with_fileglob:
    - "images/*.tar.gz"
```

Tous deux bouclent sur un lookup, comme toute forme `with_<name>` : `with_first_found` parcourt ses termes jusqu'à ce que l'un nomme un fichier qui existe, et `with_fileglob` collecte toutes les correspondances d'un motif glob. `with_first_found` cherche aussi dans un sous-répertoire de chaque entrée de recherche avant l'entrée elle-même : `templates/` quand l'action de la tâche nomme `template`, `vars/` quand elle nomme `include_vars`, `files/` sinon, soit le répertoire qu'une tâche `template:` ou `include_vars:` ordinaire chercherait d'elle-même.

Ne rien trouver fait échouer la tâche avec la phrase propre à ansible-core, `No file was found when using first_found.`, sauf si le terme est un mapping avec `skip: true`, auquel cas la boucle s'exécute à vide. Un terme rendu non défini est écarté au lieu de faire échouer tout le lookup, ce qui permet à un rôle de lire un fact qu'aucun hôte n'a encore défini et d'atteindre quand même son `when`. Un chemin trouvé par l'un ou l'autre lookup qui contient encore un marqueur de modèle (`{{`, `{%` ou `{#`) est refusé par son nom au lieu d'être rendu une seconde fois.

Un chemin qu'un lookup a trouvé sur le contrôleur est le contenu propre du playbook ; un élément construit en partie à partir d'une valeur d'hôte, comme un terme qui lit `ansible_distribution`, est lié comme une donnée, tout comme un élément issu d'un `loop:` sur une valeur enregistrée.

## Réessayer avec until

```yaml
- name: Wait for the service to answer
  command: curl -fsS http://localhost:8080/health
  register: health
  until: health.rc == 0
  retries: 10
  delay: 3
```

`until` relance une tâche jusqu'à ce que son expression soit vraie.

- `retries` est le nombre de nouvelles tentatives après la première, pas le total : la tâche s'exécute jusqu'à `retries + 1` fois. Il vaut 3 quand seul `until` est écrit, soit jusqu'à quatre exécutions.
- Un `retries` inférieur à 1 désactive les nouvelles tentatives : la tâche s'exécute une fois et son résultat n'a pas d'`attempts`.
- Volant attend `delay` secondes, 5 par défaut, après chacune des `retries` premières exécutions en échec, jamais après la dernière.
- Le résultat porte le nombre de nouvelles tentatives réellement exécutées sous `attempts`, au plus `retries`. Une tâche qui épuise ses tentatives a échoué, même si le module lui-même a réussi. Avec un `failed_when`, c'est son résultat à la dernière exécution qui décide : `failed_when: false` laisse passer la tâche, et le résultat enregistré dit toujours `failed: true`.
- `retries` sans `until` répète la tâche tant que son résultat est en échec.
- Une tâche portée par un plugin d'action (`copy`, `dnf`, `package`...) relance toute sa séquence de sous-tâches depuis le début à chaque tentative, avec un plugin neuf et tout fichier renvoyé ; `until` ne lit que le résultat de la dernière sous-tâche.

:::note
Trois nouvelles tentatives avec le délai par défaut font quinze secondes d'attente entre les quatre exécutions, comme dans Ansible.
:::

Une tâche en boucle réessaie chaque élément séparément et termine les tentatives d'un élément avant de commencer le suivant.

`until`, `retries` et `delay` sont rejetés sur `include_tasks` et `include_role`, avant la première connexion. Ansible les y rejette aussi, quand il charge le playbook. `include_vars` est un module ordinaire et réessaie comme n'importe quelle autre tâche.

## Différences avec Ansible

- Une expression `until` qui ne peut pas être évaluée est signalée avec le préfixe d'ansible-core et le message propre à Volant, de la même façon qu'un `when` en échec.
- Un `loop_control.label` qui échoue pour une raison autre qu'une variable non définie, comme un lookup qui lève une erreur, fait échouer la tâche avant l'exécution de tout élément. ansible-core termine la tâche après l'exécution de cet élément.
