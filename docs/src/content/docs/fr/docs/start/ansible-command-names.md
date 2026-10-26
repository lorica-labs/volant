---
sourceHash: 06ef4c920b21
title: Les noms des commandes ansible
description: Quelle commande de Volant remplace chaque programme d'ansible-core, ce qui est identique, ce qui diffère, et comment garder vos lignes de commande.
---

Volant exécute les neuf programmes d'ansible-core 2.19.12. Chacun est une sous-commande de `volant`, et sous Linux et macOS il peut aussi être lancé sous son nom ansible. Windows n'est pris en charge ni pour les noms de programme ni pour les liens.

## Ce qui remplace quoi

| Programme d'ansible-core | Commande de Volant | S'exécute dans | Runtime géré |
|---|---|---|---|
| `ansible` | [`volant adhoc`](/fr/docs/reference/cli/adhoc/) | Volant | inutile pour les modules natifs |
| `ansible-playbook` | [`volant playbook`](/fr/docs/reference/cli/), `volant-playbook` | Volant | installé à la première utilisation qui a besoin de Python |
| `ansible-vault` | [`volant vault`](/fr/docs/reference/cli/vault/) | Volant | inutile |
| `ansible-inventory` | [`volant inventory`](/fr/docs/reference/cli/inventory/) | Volant | inutile, sauf pour les plugins d'inventaire des collections |
| `ansible-doc` | [`volant doc`](/fr/docs/reference/cli/doc/) | Volant | inutile pour les modules natifs |
| `ansible-config` | [`volant config`](/fr/docs/reference/cli/config/) | l'outil propre d'ansible-core | installé à la première utilisation |
| `ansible-galaxy` | [`volant galaxy`](/fr/docs/reference/cli/galaxy/) | l'outil propre d'ansible-core | installé à la première utilisation |
| `ansible-pull` | [`volant pull`](/fr/docs/reference/cli/pull/) | l'outil propre d'ansible-core, avec le moteur de Volant pour ses deux étapes | installé à la première utilisation |
| `ansible-console` | [`volant console`](/fr/docs/reference/cli/console/) | l'outil propre d'ansible-core | installé à la première utilisation |

Le runtime géré s'installe lui-même la première fois qu'une commande en a besoin, comme pour un playbook, et affiche une ligne sur l'erreur standard quand il le fait. [Le runtime géré](/fr/docs/reference/runtime/) le décrit. Vous n'avez pas besoin d'installer ansible, mais quatre des neuf commandes exécutent le code propre d'ansible-core depuis ce runtime.

## Garder vos lignes de commande

Les liens ne sont pas créés si vous ne le demandez pas. Lancez le script d'installation avec `--shims`, ou lancez [`volant shims install`](/fr/docs/reference/cli/shims/) :

```sh
curl -fsSL https://volant.sh/install.sh | sh -s -- --shims
volant shims install
```

Chaque nom est un lien symbolique vers `volant`. Volant lit le nom de fichier sous lequel il a été lancé, si bien que le lien `ansible-vault` lance `volant vault`. Les deux commandes prennent les mêmes arguments et donnent la même sortie et les mêmes codes de sortie. Une erreur d'usage sort avec 2 et nomme le programme tel qu'il a été lancé, `ansible-vault: error: ...`.

`install` ne remplace pas un fichier ou un lien qu'il n'a pas créé, sauf avec `--force`. Quand un autre programme de ce nom se trouve dans votre `PATH`, il le dit : `note: /usr/bin/ansible-vault is hidden by this link, DIR comes first in PATH` quand le lien l'emporte, ou `note: ... still runs first, DIR comes later in PATH` sinon. Pour supprimer les liens :

```sh
volant shims remove
```

`remove` ne supprime que les liens qui pointent vers un fichier appelé `volant`, et laisse tout le reste.

Un script de CI comme celui-ci s'exécute tel quel une fois les liens en place, sans ansible sur la machine :

```sh
ansible-inventory -i hosts.ini --list > inventory.json
ansible-vault view group_vars/all/secrets.yml --vault-password-file ci-pass
ansible-playbook -i hosts.ini site.yml
```

## Ce qui est identique

Pour `ansible`, `ansible-vault` et `ansible-inventory`, la sortie, les options listées sur les pages des commandes et les codes de sortie ont été mesurés contre ansible-core 2.19.12. Chaque affirmation a un test derrière elle.

| Affirmation | Où elle est vérifiée |
|---|---|
| `volant adhoc` affiche les octets du callback `minimal`, le bloc `[ERROR]` et les formes `-o` et `-t`, avec les codes de sortie de la référence | les goldens de scénario `adhoc-*` et `crates/volant/tests/adhoc_cli.rs` |
| Le nom `ansible` lance `volant adhoc` | `the_ansible_name_runs_adhoc` |
| Le nom `ansible-playbook` lance `volant playbook` | `a_link_named_ansible_playbook_behaves_as_the_playbook_subcommand` |
| `volant vault` donne les messages, les codes de sortie, les modes de fichier et les fichiers d'ansible-vault | `crates/volant/tests/vault_cli.rs`, et les goldens de scénario `vault-*` pour les données vault qu'un playbook lit |
| Un fichier que l'un des outils chiffre, l'autre le déchiffre, pour l'id par défaut et pour un label | `a_file_one_tool_encrypts_the_other_decrypts` |
| `volant inventory` affiche les octets JSON et YAML d'ansible-inventory : ordre des clés, indentation de quatre espaces, ancres PyYAML | `crates/volant/tests/inventory_golden.rs` sur `golden/inventories/cases.yml`, et `inventory_cli.rs` |
| `volant config` et `volant galaxy` affichent ce qu'affiche l'outil | `crates/volant/tests/tools_cli.rs` |
| `volant pull` affiche ce qu'affiche `ansible-pull`, sauf une ligne (ci-dessous) | `a_pull_prints_what_the_reference_pull_prints` dans `console_pull.rs` |
| `volant console` affiche sur un tube ce qu'affiche `ansible-console` pour ses commandes, ses modules et ses lignes shell, et n'a pas besoin de Python pour les modules natifs | `the_console_reads_its_commands_from_a_pipe` et `the_console_runs_without_python` dans `console_pull.rs` |
| Les liens que crée `install`, les lignes qu'il affiche et ce que `remove` laisse | `crates/volant/tests/shims.rs` |
| Quels noms correspondent à quelle commande, et aucun autre | les tests de `crates/volant/src/cli/program.rs` |

Les options que chaque commande native accepte sont listées sur sa propre page : [`volant adhoc`](/fr/docs/reference/cli/adhoc/), [`volant vault`](/fr/docs/reference/cli/vault/) et [`volant inventory`](/fr/docs/reference/cli/inventory/). Une option qui n'y figure pas est refusée, jamais ignorée.

## Ce qui diffère

- Le texte d'aide est celui de Volant. Il n'est jamais comparé à la référence.
- `--version` affiche `ansible [core 2.19.12]` (ou le nom du programme) et les chemins. Les lignes Python, Jinja et PyYAML sont omises quand Volant ne trouve aucun interpréteur avec ansible-core, parce que `--version` n'installe rien. Voir [`volant adhoc`](/fr/docs/reference/cli/adhoc/).
- `executable location` dans `--version` est le programme tel qu'il a été lancé : `volant`, ou le lien. Voir [`volant doc`](/fr/docs/reference/cli/doc/#terminal-code-de-sortie-et---version).
- `ansible-inventory --toml` n'a besoin d'aucun paquet Python, là où la référence demande `tomli-w`, et une valeur `null` donne une ligne `[ERROR]` au lieu d'une trace Python (les deux sortent avec 250). Voir [`volant inventory`](/fr/docs/reference/cli/inventory/).
- `volant pull` affiche `<cache>/volant/pull/ansible-pull` sur la deuxième ligne de sa sortie, là où `ansible-pull` affiche le chemin de son script. Voir [`volant pull`](/fr/docs/reference/cli/pull/).
- `volant config dump` montre `COLLECTIONS_PATHS` comme fixé par l'environnement dès que le runtime a un répertoire `collections/`. Voir [`volant config`](/fr/docs/reference/cli/config/#le-chemin-des-collections).
- `ansible-vault view` passe par un pager seulement sur un terminal, avec `PAGER`, puis `ANSIBLE_PAGER`, puis `[defaults] pager`, puis `less`. Voir [Vault](/fr/docs/variables/vault/).
- `volant adhoc` et `volant playbook` utilisent le moteur de Volant : les différences listées dans [Mon playbook va-t-il s'exécuter ?](/fr/docs/start/compatibility/) et sur [`volant adhoc`](/fr/docs/reference/cli/adhoc/#les-actions-qui-ne-sont-pas-des-modules) s'appliquent.

## Codes de sortie

| Commande | 0 | 1 | 2 | 4 | 5 | 250 |
|---|---|---|---|---|---|---|
| `volant adhoc` | chaque hôte a réussi ou a été ignoré | aucun hôte à cibler, ou une mauvaise action `meta` | un hôte a échoué, ou une erreur d'usage | un hôte était injoignable, ou quelque chose que cette version ne prend pas encore en charge | un réglage invalide, ou aucun argument pour `command` | un argument qui n'est pas du JSON valide |
| `volant vault` | terminé | une erreur de fichier, comme `create` sur un fichier existant | une erreur d'usage | | une mauvaise option, comme deux fichiers avec `--output` | |
| `volant inventory` | affiché | | une erreur d'usage | | aucune action ou deux actions, ou une mauvaise cible de `--host` ou `--graph` | |

`volant playbook` utilise les codes de [Codes de sortie](/fr/docs/reference/exit-codes/). Les quatre outils qui s'exécutent dans le runtime d'ansible-core renvoient le code propre de l'outil, ou 1 avec une ligne `[ERROR]:` quand l'interpréteur ne peut pas être démarré.
