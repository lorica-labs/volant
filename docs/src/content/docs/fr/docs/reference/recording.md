---
sourceHash: 121b3af3c375
title: Enregistrements de runs
description: Le fichier qu'écrit le callback volant.compat.record pendant un run d'ansible-playbook, ligne par ligne.
---

Avant de changer d'outil, une équipe peut enregistrer ce que fait `ansible-playbook` sur ses vrais runs, puis faire rejouer ces runs par Volant. C'est le callback `volant.compat.record` qui écrit l'enregistrement. Il est livré dans la collection `volant.compat`, n'a besoin que de la bibliothèque standard de Python et fonctionne avec ansible-core 2.19 et les versions suivantes.

## L'activer

Installez la collection là où ansible-core trouve ses collections, puis ajoutez deux sections à `ansible.cfg` :

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

C'est un callback de notification. Il ne remplace jamais le callback de sortie standard, et la sortie du run comme son code de retour restent ceux d'un run sans lui.

| Réglage | Signification |
|---|---|
| `path` | Le fichier à écrire. `{{ timestamp }}` devient l'heure de début du run, en UTC, sous la forme `20261025T093000Z` ; aucun autre template n'est lu. Un nom qui finit par `.gz` est compressé avec gzip. Les répertoires parents manquants sont créés avec le mode `0700`. La valeur par défaut est `shadow/run-{{ timestamp }}.jsonl.gz`. |

`VOLANT_RECORD_PATH` règle `path` depuis l'environnement, ce qui convient à un job de CI.

Quand le fichier ne peut pas être écrit, le callback affiche un avertissement et le run continue sans enregistrement.

## Sécurité

Un enregistrement contient les résultats des tâches tels qu'ansible-core les a vus. Il peut contenir des valeurs de vault déchiffrées, les variables extra du run et tout ce qu'un module a renvoyé. Traitez-le comme un secret :

- Le fichier est créé avec le mode `0600`, et un fichier existant est remis à `0600`.
- Gardez le répertoire des enregistrements hors du contrôle de version, et conservez les enregistrements de CI comme artefacts privés, avec une rétention courte.
- Une tâche avec `no_log` garde son statut, et son résultat est enregistré sous la forme `{"censored": true}`.

## Format

Un enregistrement est au format JSON Lines : un objet JSON par ligne, chacun avec une clé `event`. Cette page décrit le format `1`.

| `event` | Champs |
|---|---|
| `header` | `format`, `ansible_core` (sa version), `cli` (les options de ligne de commande du run, telles qu'ansible-core les a lues), `cwd`, `playbooks` (le `path` et le `sha256` de chaque playbook), `inventory` (les `sources` telles que données, et le `path` et le `sha256` de chaque source qui est un fichier), `extra_vars_hash`, `started` |
| `inventory` | `hosts` : chaque hôte de l'inventaire, avec les groupes auxquels il appartient en dehors de `all` |
| `play` | `uuid`, `name`, `hosts` (le motif du play) |
| `task` | `uuid`, `path` (`fichier:ligne`), `name`, `action` (le nom complet du module ou de l'action), `handler`, `role` |
| `result` | `host`, `task` (son `uuid`), `item`, `status`, `delegated_to`, `result` |
| `retry` | `host`, `task`, `attempt`, `result` : une tentative échouée d'une boucle `until` |
| `include` | `file`, `hosts` : un fichier qu'`include_tasks` ou `include_role` a ajouté, et les hôtes qui l'exécutent |
| `notify` | `handler`, `host` : un handler qui va s'exécuter sur un hôte |
| `stats` | `hosts` (le récapitulatif de chaque hôte), `custom` (ce que `set_stats` a rassemblé) |

Les lignes `header` et `inventory` viennent en premier, au début du premier play. `stats` est la dernière ligne d'un run complet : un enregistrement sans elle vient d'un run interrompu.

Notes sur les champs :

- Chaque empreinte est un SHA-256 en hexadécimal. `extra_vars_hash` est calculé sur les variables extra en JSON, clés triées.
- `status` vaut `ok`, `changed`, `failed`, `skipped` ou `unreachable`. Un échec qu'`ignore_errors` a laissé passer reste `failed`.
- Dans une boucle, chaque élément a une ligne `result` dont `item` est son index à partir de 0. La ligne qui suit, avec `item` à `null`, est la tâche entière, avec les éléments sous `results`.
- `delegated_to` est l'hôte nommé par un `delegate_to`, ou `null`.
- `result` est le résultat de la tâche tel que les callbacks le reçoivent, y compris les clés qu'ajoute ansible-core, comme `invocation`, `false_condition`, `skip_reason` et `attempts`. La présence de ces clés dépend de la version : ansible-core 2.21 ne transmet plus `invocation` aux callbacks.
- Le résultat de la collecte des facts est enregistré en entier, parce qu'un rejeu rend les templates avec. Cela fait 50 à 100 Ko par hôte, d'où gzip par défaut.
- `include_tasks` produit une ligne `task` et un `result` par hôte pour lui-même, puis une ligne `include` par fichier. `import_tasks` ne produit ni l'un ni l'autre, seulement les tâches importées.
- Les lignes `notify` sont écrites quand les handlers s'exécutent, après les tâches qui les ont notifiés.
