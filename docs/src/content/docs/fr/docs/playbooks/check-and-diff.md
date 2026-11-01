---
sourceHash: 4a0054ff06cb
title: Mode check, diff et listes
description: Ce que font --check et --diff, quels modules y répondent, comment les tags sélectionnent les tâches, et les commandes qui décrivent une exécution sans la démarrer.
---

Cette page répond à la question suivante : comment prévisualiser une exécution avant qu'elle ne change quoi que ce soit. Elle couvre les simulations avec `--check`, les changements de fichiers avec `--diff`, la sélection de tâches par tags et les listes qui ne se connectent à rien.

`--check` indique ce que chaque tâche changerait sans rien changer. `--diff` affiche les changements qu'un module apporte aux fichiers. Les deux fonctionnent comme dans `ansible-playbook` 2.19.12, et le récapitulatif et le code de sortie correspondent.

Les mots-clés de tâche l'emportent sur les options. `check_mode: false` exécute la tâche pour de vrai sous `--check`. `check_mode: true` fait d'une tâche une simulation dans un play normal. `diff: false` garde les changements d'une tâche hors de l'écran sous `--diff`.

`ansible_check_mode` et `ansible_diff_mode` ne reflètent que `--check` et `--diff` de la ligne de commande. Le mot-clé `check_mode` ou `diff` propre à un play ou à une tâche ne les change pas.

## Ce qui s'exécute où

Une tâche en mode check ne change jamais un hôte, quel que soit le chemin qui l'exécute.

Volant embarque une version native de certains modules, écrite en Rust et exécutée dans l'agent, le petit programme qu'il envoie à chaque hôte. Quand la version native ne peut pas répondre à une tâche, elle la rend : le code Python du module s'exécute alors sur l'hôte.

| Tâche | Ce qui se passe |
|---|---|
| `command`, `shell`, `raw` | Sautées, comme dans ansible-core. Un `command` ou `shell` avec `creates` ou `removes`, dans la ligne ou dans `args:`, est vérifié : `ok` quand la garde l'arrête, `changed` quand il s'exécuterait. |
| `stat`, `setup`, `package_facts`, `service_facts` | Traités par l'agent. Ils ne font que lire : check et diff ne changent rien. |
| `file`, `lineinfile`, `systemd`, `user`, `group`, `apt` | Rendus au code Python du module sur l'hôte, qui sait ce qu'il aurait changé et comment formuler le diff. |
| `copy`, `template`, `fetch`, `unarchive` et d'autres plugins d'action | Exécutés par le propre plugin d'ansible-core via l'assistant Python du contrôleur (un processus qui exécute ansible-core sur le contrôleur), qui calcule lui-même le résultat du check et le diff. |
| Modules du contrôleur (`debug`, `set_fact`, `assert`, ...) | Exécutés comme d'habitude. |
| `package`, `service`, `dnf` | Choisissent le module pour l'hôte, puis ce module s'exécute en mode check. |

Le profil (`--profile`) nomme la raison de chaque renvoi : "the task runs in check mode" ou "the task shows a diff".

## Un hôte sans Python

Le renvoi a besoin de Python sur l'hôte. Sans lui, tout module natif qui change l'hôte échoue sous `--check` ou `--diff` : `apt`, `user`, `group`, `file`, `lineinfile`, `systemd`, `systemd_service` et le module `copy` que le plugin copy exécute. Installez Python sur l'hôte, ou exécutez le play sans ces options. Les tâches qui ne font que lire (`stat`, `setup`, `package_facts` et `service_facts`), ainsi que `command`, `shell` et `raw`, fonctionnent toujours.

## Sortie du diff

Un diff ne s'affiche que pour une tâche qui a changé quelque chose. `copy` et `template` n'affichent rien quand `diff` est désactivé. Le processus auxiliaire ne démarre que lorsqu'un diff est affiché.

## Limites connues

- `-vvvv` liste les options qu'ansible-core liste, dans son ordre, et les lignes de mise en place des plugins. Ses lignes ssh montrent les commandes que ce moteur exécute par `ssh` (la sonde, l'envoi et le lien vers l'agent), pas l'envoi `sftp` d'ansible-core, et une connexion locale ou un fichier de module n'affiche aucune ligne.

La [page de parité](/fr/docs/reference/parity/) liste chaque option et chaque mot-clé avec son statut.

## Tags

`tags` peut s'écrire sur un play, une entrée de rôle, un bloc ou une tâche. Le compilateur replie les tags extérieurs dans chaque tâche en dessous. `--tags` et `--skip-tags` décident ensuite quelles tâches sont compilées : les tags décident donc à la fois de ce qui s'exécute et de ce que les listes montrent.

### Les règles des tags

Volant applique exactement les règles d'Ansible :

| Tag sur la tâche | Comportement |
|---|---|
| `always` | Sélectionnée par tout `--tags`. Survit à `--skip-tags all` sauf si `always` est lui-même sauté. |
| `never` | Écartée de `--tags all` et `--tags tagged`. Ne revient que lorsqu'une sélection la nomme. |
| aucun tag | Traitée comme le tag `untagged`, c'est pourquoi `--tags untagged` la sélectionne. |

`[tags] run` et `[tags] skip` dans `ansible.cfg`, ainsi que `ANSIBLE_RUN_TAGS` et `ANSIBLE_SKIP_TAGS`, sont des valeurs par défaut. La ligne de commande s'y ajoute au lieu de les remplacer.

Les tags écrits sur un `include_tasks` ou un `include_role` s'arrêtent à l'instruction : `--tags inc` exécute l'include, pas ce qu'il apporte. Les tags du play, de l'entrée de rôle et des blocs autour de l'instruction atteignent bien les tâches incluses : `--tags nginx` sur un rôle taggé `nginx` exécute donc les fichiers que ce rôle inclut. Ansible se comporte de la même façon.

### `ansible_run_tags` et `ansible_skip_tags`

Une tâche peut relire la sélection de tags de sa propre exécution sous forme de deux variables : `ansible_run_tags` contient `--tags` (`["all"]` quand il n'a pas été donné) et `ansible_skip_tags` contient `--skip-tags` (`[]` quand il n'a pas été donné), chacune après intégration de `[tags] run` / `[tags] skip` d'`ansible.cfg` et des deux variables d'environnement `ANSIBLE_RUN_TAGS` / `ANSIBLE_SKIP_TAGS`.

```yaml
- name: Only fetch the kubeconfig once it has been asked for
  fetch:
    src: /etc/rancher/k3s/k3s.yaml
    dest: "{{ kubeconfig }}"
  when: "'kubeconfig' in ansible_run_tags"
```

## Regarder avant d'exécuter

Quatre options lisent un playbook et affichent ce qu'une exécution ferait, sans se connecter à rien :

```sh
volant playbook -i inventory.ini site.yml --list-hosts
volant playbook -i inventory.ini site.yml --list-tasks
volant playbook -i inventory.ini site.yml --list-tags
volant playbook -i inventory.ini site.yml --syntax-check
```

Leur sortie correspond à celle d'Ansible jusqu'aux espaces. La suite de tests la compare octet par octet avec ansible-core sur un corpus de playbooks.

Les listes ne parcourent que le corps d'un bloc : une tâche dans `rescue:` ou `always:` n'apparaît donc pas, exactement comme dans Ansible.

## Différences avec Ansible

- Un play avec plusieurs tags les affiche triés, et `--list-hosts` affiche les hôtes d'un play dans l'ordre de l'inventaire. Ansible affiche un set Python aux deux endroits, et son ordre change d'une exécution à l'autre.
- Une liste ne résout pas les modules. Un playbook dont cette version ne sait pas exécuter les modules s'affiche quand même et passe `--syntax-check`, ce qui permet de lire un rôle avant la version qui l'exécute.
- Une tâche dont `ignore_errors` ou `become` n'est ni vrai ni faux fait terminer une liste avec le code 4. Ansible la liste et se termine avec 0. La valeur est un playbook mal formé, pas une fonctionnalité non implémentée.
