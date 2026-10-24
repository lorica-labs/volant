---
sourceHash: 85339820dcff
title: Quand une exécution échoue
description: Trouvez la tâche qui a échoué et pourquoi, vérifiez le même playbook sous ansible-playbook, et signalez ce que vous avez trouvé.
---

Partez du code de sortie. Il vous dit si l'exécution s'est arrêtée avant de toucher un hôte ou si elle a échoué sur l'un d'eux.

| Code | Ce qui s'est passé | Où regarder |
|---|---|---|
| 4, sans récapitulatif | Le playbook s'est arrêté avant la première connexion. Il ne s'analyse pas, ou il utilise quelque chose que cette version ne prend pas encore en charge. | Le message au-dessus du code. Voir [Arrêt avant l'exécution](#arrêt-avant-lexécution). |
| 2, 4 ou 6, avec un récapitulatif | Un hôte a échoué (2), un hôte était injoignable (4), ou les deux (6). | La ligne `fatal:` de la tâche, puis [Échec pendant l'exécution](#échec-pendant-lexécution). |
| 1 | Un rôle, un playbook ou un motif d'hôtes est introuvable. | Le fichier ou le nom que cite le message. |

[Codes de sortie](/fr/docs/reference/exit-codes/) liste tous les codes.

## Arrêt avant l'exécution

Presque tout ce que cette version ne sait pas exécuter est nommé avant que Volant ne se connecte à quoi que ce soit. Le message dit ce qu'il a refusé et où :

| Message | Que faire |
|---|---|
| `task 'NAME': keyword 'KEYWORD' is not supported yet on 'MODULE'` | Vérifiez le mot-clé dans [Mots-clés](/fr/docs/reference/keywords/). Retirez-le, ou gardez ce play sous `ansible-playbook` pour l'instant. |
| `task 'NAME': argument 'ARGUMENT' is not supported yet on 'MODULE'` | Vérifiez le module dans [Modules](/fr/docs/reference/modules/). La plupart des arguments refusés y ont une solution de contournement. |
| `couldn't resolve module/action 'NAME'` | Vérifiez l'orthographe. Si le module vient d'une collection, installez-la avec `volant runtime install --collections NAMESPACE.COLLECTION`. |
| `strategy 'NAME' is not supported yet` | Utilisez `linear`, `free`, `host_pinned` ou `debug`. |
| `become_method 'NAME' is not supported yet` | Utilisez `sudo` ou `su`. Celui-ci se termine avec le code 2. |
| `the role 'NAME' was not found in ...` | Le message liste les répertoires explorés. Ajoutez le répertoire du rôle à `roles_path`. Celui-ci se termine avec le code 1. |

[Mon playbook va-t-il s'exécuter ?](/fr/docs/start/compatibility/#ce-qui-est-refusé-aujourdhui) liste ces refus et montre comment vérifier un projet entier sans l'exécuter. [La vérification préalable](/fr/docs/internals/preflight/) explique comment cette vérification fonctionne.

## Échec pendant l'exécution

1. Relancez sur l'hôte en échec seulement, avec plus de sortie. `-vvv` affiche les arguments reçus par chaque module :

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 -vvv
   ```

2. Sautez les tâches qui passent déjà. `--start-at-task` démarre à la tâche que vous nommez, et `--step` demande confirmation avant chaque tâche :

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 --start-at-task "Install nginx"
   ```

3. Arrêtez-vous sur l'échec et regardez autour. Avec `debugger: on_failed` sur le play ou la tâche, Volant ouvre le débogueur de tâches d'Ansible quand une tâche échoue. Vous pouvez afficher des variables, changer les arguments et relancer la tâche.

## Comparer avec ansible-playbook

Volant lit les mêmes fichiers : la même commande s'exécute donc sous Ansible :

```sh
ansible-playbook -i inventory.ini site.yml --limit web1
```

Si Ansible échoue de la même façon, le problème est dans le playbook ou sur l'hôte. Si seul Volant échoue, ou si les deux donnent des résultats différents, c'est un bug de Volant. Vous pouvez garder le play sous `ansible-playbook` le temps de le corriger.

## Revenir à ansible-playbook

Les mêmes fichiers s'exécutent sous `ansible-playbook` sans changement. Volant n'ajoute rien à vos playbooks, rôles, inventaires ni à `ansible.cfg` : il n'y a donc rien à reconvertir.

Sur les hôtes, la seule chose qui reste est l'agent en cache : un répertoire appelé `volant-agent-<version>-<uid>` dans `~/.ansible/tmp`, et un répertoire `volant-blobs-<version>-<uid>` à côté si une exécution a utilisé des modules Python. Pour les supprimer, effacez-les sur chaque hôte, avec le compte que l'exécution a utilisé :

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-*'
```

Un hôte où des tâches se sont exécutées avec `become` contient une seconde copie dans le répertoire personnel du compte vers lequel il a élevé ses privilèges. Lancez la même commande avec ce compte. Pour retirer aussi Volant du contrôleur, voir [Désinstaller](/fr/docs/start/installation/#désinstaller). [Ce que Volant laisse sur vos machines](/fr/docs/#ce-que-volant-laisse-sur-vos-machines) liste tout.

## Le signaler

- Une différence avec `ansible-playbook` va dans un rapport d'[écart de compatibilité Ansible](https://github.com/lorica-labs/volant/issues/new?template=compat.yml).
- Tout le reste qui ne fonctionne pas comme documenté va dans un [rapport de bug](https://github.com/lorica-labs/volant/issues/new?template=bug.yml).
- Un problème de sécurité passe par la [politique de sécurité](https://github.com/lorica-labs/volant/blob/main/SECURITY.md), jamais par un ticket public.

Joignez la sortie de `volant --version`, le plus petit playbook et inventaire qui échouent encore, et la sortie de l'exécution en échec avec `-vvv`. Retirez les mots de passe, les clés, le contenu vault et les noms d'hôtes avant de les publier. Un ticket public reste public.
