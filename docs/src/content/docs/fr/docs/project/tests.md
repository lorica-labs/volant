---
sourceHash: faf55681f8db
title: Tests
description: Comment Volant est vérifié par rapport à ansible-core, les quatre types de réponse enregistrée, et comment ajouter un scénario.
---

Volant doit se comporter comme ansible-core 2.19.12. La plus grande partie de la suite de tests est donc une comparaison : ansible-core a exécuté quelque chose une fois, sa réponse a été enregistrée dans le dépôt, et le test lance Volant sur la même entrée et compare.

## Les quatre types

Chaque type d'enregistrement a un nom, et `just golden <kind>` le régénère.

| Type | Ce qui a été enregistré | Où il s'exécute |
|---|---|---|
| `template` | Le résultat du rendu de chaque expression de `crates/volant/tests/golden/cases.yml` | Tous les jobs, avec `reference.rs` |
| `inventory` | Les hôtes, variables et groupes d'un inventaire, et les hôtes que sélectionne chaque motif | Tous les jobs, avec `reference.rs` |
| `scenario` | L'exécution d'un playbook : le code de sortie et toute la sortie standard | Tous les jobs, avec `reference.rs` |
| `module` | Ce que chaque module natif a répondu sur une vraie machine | Le job `ssh` et `just test-reference`, avec `reference_modules.rs` |

Rien dans un enregistrement n'est modifié à la main. Changez l'entrée, lancez `just golden <kind>` avec ansible-core 2.19.12 installé, et lisez le diff. Le lancer sur un arbre inchangé laisse les fichiers exactement comme ils étaient. Le type `module` a aussi besoin de `sudo`, de systemd, d'apt et de cron, parce qu'il crée un compte et une unité sur la machine.

## Ajouter un scénario

Un scénario est un répertoire sous `crates/volant/tests/scenarios` :

1. Créez `crates/volant/tests/scenarios/<name>/` et mettez-y le playbook, l'inventaire et tous les fichiers que le playbook lit.
2. Écrivez `case.yml` à côté. Une seule exécution s'écrit `args: [-i, inv.ini, site.yml]`. Plusieurs exécutions du même répertoire s'écrivent `runs:`, une liste de `{args: [...]}`.
3. Lancez `just golden scenario`. Cela écrit `expected.json` dans le répertoire.
4. Lancez `touch crates/volant/tests/reference.rs` puis les tests. La liste des scénarios est fixée quand ce fichier est compilé : un nouveau répertoire n'est donc pas vu avant une recompilation.

Chaque exécution de Volant a lieu dans une copie neuve du répertoire, avec `--forks 1` sauf si le cas dit autre chose. Le test compare le code de sortie et toute la sortie standard, durées masquées. Outre `args` et `runs`, `case.yml` accepte cinq autres clés :

- `modes` liste `check`, `check-diff` et `diff`. Chaque exécution a alors lieu une fois de plus par mode, avec `--check`, `--check --diff` ou `--diff` ajouté, dans cet ordre fixe, et chaque exécution est enregistrée.
- `env` définit des variables d'environnement pour l'exécution.
- `forks` relève le nombre de forks au-dessus de un. Les hôtes peuvent alors afficher dans un ordre différent : listez donc les lignes dont l'ordre peut différer dans `unordered`, sous forme d'expressions régulières. `unordered` sans `forks` est refusé.
- `needs` nomme ce que la machine doit avoir : `ansible-core`, `sudo` ou `systemd`. Un cas dont les besoins ne sont pas satisfaits est ignoré, sauf quand `VOLANT_PYTHON` est défini, ce qui transforme l'omission en échec.
- `links` associe un chemin à la cible d'un lien que chaque copie reçoit à ce chemin. Utilisez-le pour un lien que git ou cargo ne peuvent pas contenir, comme un lien vers un fichier qui n'existe pas : `cargo package` refuse un lien suivi de ce genre.

## La matrice de parité

`crates/volant/tests/parity.rs` compare chaque nom que connaissent ansible-core 2.19.12 et les collections du paquet `ansible` fixé (mots-clés, modules, filtres, tests, lookups et les autres types de plugins) avec ce que le moteur en fait. `just parity-reference` régénère le côté référence, `crates/volant/tests/parity/reference.json`. Les pages [Parité](/fr/docs/reference/parity/), [Mots-clés](/fr/docs/reference/keywords/) et [Modules](/fr/docs/reference/modules/) sont générées à partir des tables propres du moteur par `just docs-parity`, `just docs-keywords` et `just docs-modules`, et un test échoue quand l'une d'elles est périmée.

## Les lancer

`just check-touched` est la vérification à lancer avant une pull request. Elle formate, lance clippy et le cliquet de taille de fichier, lance les tests des modules que la branche change, et vérifie l'orthographe. `just check-local` fait la même chose avec tous les tests. La CI lance tous les tests.

`just test` lance tout ce qui n'a besoin d'aucun hôte. `just test-reference` lance les enregistrements de modules contre la machine sur laquelle vous êtes, ce qui demande ansible-core, sudo et systemd. `just ssh-test` est ce que lance le job `ssh` : il part d'un sshd sur localhost et couvre aussi les tests de bout en bout. Préférez un scénario ou une ligne de table dans un test existant à une nouvelle fonction de test.
