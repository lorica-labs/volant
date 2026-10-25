---
sourceHash: 8577d9c13ab0
title: Priorité et facts
description: Quelle source l'emporte quand deux définissent la même variable, les variables magiques, et comment les facts sont collectés, nommés et mis en cache.
---

Cette page répond à deux questions : quelle valeur a une variable quand plusieurs sources la définissent, et d'où viennent les facts d'un hôte.

Quand deux sources définissent la même variable, celle qui est plus bas dans cette liste l'emporte. L'ordre est celui d'ansible-core 2.19, chaque couche de rôle ayant été mesurée par rapport à ses voisines.

1. Le `defaults/main.yml` d'un rôle
2. `group_vars/all`, depuis le répertoire de l'inventaire puis le répertoire du playbook
3. Les variables de groupe de l'inventaire, appliquées par profondeur de groupe puis par nom
4. `group_vars/<group>`, depuis le répertoire de l'inventaire puis le répertoire du playbook
5. Les variables d'hôte de l'inventaire
6. `host_vars/<host>`, depuis le répertoire de l'inventaire puis le répertoire du playbook
7. Les facts collectés sur l'hôte
8. Les `vars` du play
9. Les `vars_files` du play
10. Le `vars/main.yml` d'un rôle
11. Les `vars` d'une tâche
12. `set_fact`, les résultats enregistrés et `include_vars`
13. Les paramètres de rôle, et les `vars:` d'une instruction d'include dynamique
14. `--extra-vars`

[Rôles](/fr/docs/playbooks/roles/#variables-de-rôle) explique d'où viennent les trois couches de rôle et en quoi elles diffèrent.

Un fichier de variables qui ne contient que `---` et des commentaires se charge comme un mapping vide, comme dans ansible-core. Un fichier qui contient un scalaire comme `42` reste refusé.

Un mapping peut reprendre les clés d'un autre avec une clé de fusion, `<<: *base`, dans tous les fichiers YAML que Volant lit : fichiers de variables, inventaires, playbooks et arguments de tâches. Les clés fusionnées viennent en premier, et une clé écrite à côté de `<<` remplace celle qui est fusionnée. Dans `<<: [*a, *b]`, le premier mapping l'emporte. Une valeur qui n'est ni un mapping ni une liste de mappings arrête le chargement, avec le message de PyYAML.

:::note
Un fact collecté perd face aux variables propres du play. Un playbook qui définit une variable portant le même nom qu'un fact lit sa propre valeur, comme dans Ansible.
:::

## Variables magiques

Un ensemble fixe de variables magiques se place au sommet de la vue de chaque hôte et l'emporte toujours :

| Variable | Contient |
|---|---|
| `inventory_hostname`, `inventory_hostname_short` | Le nom de cet hôte dans l'inventaire. |
| `group_names`, `groups` | Les groupes de cet hôte, et chaque groupe avec ses hôtes. |
| `hostvars` | Les variables de chaque hôte, voir plus bas. |
| `ansible_play_hosts`, `ansible_play_hosts_all` | Les hôtes actifs du play, et tous ses hôtes. |
| `ansible_play_batch`, `play_hosts` | Les hôtes du [lot `serial`](/fr/docs/playbooks/how-a-play-runs/#exécution-par-lots-avec-serial) courant. `play_hosts` est obsolète. |
| `playbook_dir`, `inventory_dir`, `inventory_file` | Les chemins de l'exécution courante. |
| `omit` | Le marqueur qui retire un argument de module. |
| `vars` | Toutes les variables de cet hôte, lues par leur nom. `vars['a']`, `vars[name]` et `vars.get('a')` rendent le template que la variable contient, comme `{{ a }}`. `omit` n'en fait pas partie. |
| `ansible_playbook_python` | L'interpréteur Python qui exécute ansible-core pour Volant. Trouvé quand un modèle le lit, et non défini quand on n'en trouve aucun. |
| `ansible_check_mode`, `ansible_diff_mode` | Si l'exécution a `--check` et `--diff`. Le mot-clé `check_mode` ou `diff` propre à un play ou à une tâche ne les change pas. |
| `ansible_run_tags`, `ansible_skip_tags` | Les tags que l'exécution sélectionne et saute, voir [Mode check, diff et listes](/fr/docs/playbooks/check-and-diff/#ansible_run_tags-et-ansible_skip_tags). |
| `ansible_forks` | La valeur courante de `forks`. |
| `ansible_version` | La version d'ansible-core que Volant reproduit, voir la [fiche de décision 0003](/fr/docs/decisions/0003-ansible-version-reports-the-reference-release/). |
| `volant_version` | La version propre à Volant. Testez `volant_version is defined` pour distinguer les deux moteurs. |

Dans un `rescue`, `ansible_failed_task` et `ansible_failed_result` sont définis comme facts sur l'hôte qui a échoué.

Chaque tâche reçoit aussi `ansible_search_path`, et la tâche d'un rôle reçoit `role_path`. Les deux sont en place avant la résolution des `vars:` propres à la tâche : une variable de tâche construite avec `lookup('template')` trouve donc le `templates/` du rôle. [Templating](/fr/docs/variables/templating/#ce-qui-est-disponible) liste ce que contient le chemin de recherche.

## hostvars

`hostvars` contient ce que l'inventaire, `--extra-vars`, `set_fact` et les facts collectés ont produit pour chaque hôte. Il n'inclut pas les variables de play ou de tâche d'un autre hôte. Il est reconstruit chaque fois qu'un fact change.

Chaque hôte lit une seule table partagée plutôt que sa propre copie : une tâche qui nomme `hostvars` coûte donc une consultation, pas une copie de l'inventaire.

Une tâche qui lit `hostvars`, `ansible_play_hosts` ou `ansible_play_batch` attend d'abord que tous les hôtes l'atteignent, pour voir ce qu'Ansible montrerait à ce moment-là.

## Facts

Les facts sont ce qu'un hôte rapporte sur lui-même, comme son nom, sa distribution et ses adresses.

```yaml
- hosts: web
  tasks:
    - debug:
        msg: "{{ ansible_hostname }} runs {{ ansible_facts.distribution }}"
```

Les facts collectés se classent au-dessus des variables d'inventaire et sous les variables propres du play, comme le montre la liste en haut de cette page.

### Quand les facts sont collectés

Un play collecte les facts sauf s'il définit `gather_facts: false`. Avec `gathering = explicit` dans `ansible.cfg`, seul un play qui écrit `gather_facts` en collecte. Avec `smart`, un hôte qui a des facts récents dans le [cache de facts](/fr/docs/hosts/inventories/#le-cache-de-facts) est sauté. Un modèle `gather_facts` est rendu une fois, au démarrage du play.

Volant exécute soit le module `setup` d'ansible-core par le [chemin Python préchauffé](/fr/docs/internals/python/), soit laisse l'agent (le petit programme que Volant place sur chaque hôte) collecter lui-même les facts, quand le playbook ne lit manifestement rien d'autre. Voir [Facts natifs](#facts-natifs).

Comme `setup` peut toujours se rabattre sur son module Python, le contrôleur a besoin d'ansible-core et l'hôte d'un interpréteur Python 3. Une exécution qui n'a besoin de Python que pour collecter les facts n'installe pas le [runtime géré](/fr/docs/reference/runtime/) : un contrôleur sans ansible-core utilisable arrête donc un play qui collecte des facts avant la première connexion, au lieu de continuer sans eux. Définissez `gather_facts: false` sur les plays qui n'ont pas besoin de facts.

`gather_subset` et `gather_timeout` fonctionnent sur un play, et sur le module `setup` avec `filter`, comme dans ansible-core.

### Deux noms pour chaque fact

Chaque fact est lisible à deux endroits :

- à plat, avec un préfixe `ansible_` : `ansible_hostname`
- dans `ansible_facts`, écrit comme le module l'a renvoyé : `ansible_facts.hostname`

Ce sont des variables distinctes. Un `set_fact` ultérieur d'`ansible_hostname` masque celle à plat et laisse `ansible_facts.hostname` tranquille. Ansible se comporte de la même façon.

### Facts natifs

Un collecteur de facts natif est une version écrite en Rust qui s'exécute dans l'agent : l'hôte n'a donc pas besoin de Python pour lui. Il existe pour Debian et Ubuntu. Il collecte les 17 collecteurs qu'ansible-core exécute pour `gather_subset: min`, les facts de processeur et de mémoire du collecteur `hardware` d'ansible-core, ainsi que les routes par défaut et les listes d'adresses de son collecteur `network`. Un fact que le collecteur natif ne produit pas, comme les montages, les périphériques ou les détails par interface, est omis.

Un fact absent ne compte que si quelque chose le lit. Avant la première connexion, Volant lit donc chaque play, rôle, modèle et définition de variable de l'exécution et liste les facts qu'ils peuvent lire. Il n'utilise le collecteur natif que si tous sont des facts qu'il produit. Une lecture par nom calculé, comme `ansible_facts[item]`, `vars['ansible_' ~ name]`, `lookup('vars', ...)` ou un `hostvars[host]` entier passé à un filtre, pourrait atteindre n'importe quel fact : il garde donc le `setup` d'ansible-core.

Le collecteur natif rend aussi la tâche sur un hôte qu'il ne sait pas reproduire : il abandonne et le module Python `setup` s'exécute. Cela arrive sur une autre distribution, avec SELinux activé, avec des facts locaux dans `/etc/ansible/facts.d`, ou avec un nom d'hôte que seul le DNS connaît.

`--facts python` exécute toujours le module d'ansible-core, et `--facts native` interroge toujours l'agent. `--profile` affiche lequel l'exécution a choisi et pourquoi. Voir [Ligne de commande](/fr/docs/reference/cli/#exécution-et-sortie).

### Les facts sont des données

Les facts sont les propres mots de l'hôte, ils arrivent donc non fiables : un modèle à l'intérieur d'un fact reste du texte et n'est jamais rendu. Voir [Les valeurs venues des hôtes ne sont jamais rendues comme modèles](/fr/docs/variables/templating/#les-valeurs-venues-des-hôtes-ne-sont-jamais-rendues-comme-modèles).

### Conserver les facts entre deux exécutions

Avec `fact_caching = jsonfile`, les facts persistent d'une exécution à la suivante, voir [Le cache de facts](/fr/docs/hosts/inventories/#le-cache-de-facts).
