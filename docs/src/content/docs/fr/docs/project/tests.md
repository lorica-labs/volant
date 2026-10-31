---
sourceHash: d2cbf2aab06e
title: Tests
description: Comment Volant est vérifié par rapport à ansible-core, les types de réponse enregistrée, et comment ajouter un scénario.
---

Volant doit se comporter comme ansible-core 2.19.12. La plus grande partie de la suite de tests est donc une comparaison : ansible-core a exécuté quelque chose une fois, sa réponse a été enregistrée dans le dépôt, et le test lance Volant sur la même entrée et compare.

## Les types

Chaque type d'enregistrement a un nom, et `just golden <kind>` le régénère.

| Type | Ce qui a été enregistré | Où il s'exécute |
|---|---|---|
| `template` | Le résultat du rendu de chaque expression de `crates/volant/tests/golden/cases.yml` | Tous les jobs, avec `reference.rs` |
| `inventory` | Les hôtes, variables et groupes d'un inventaire, et les hôtes que sélectionne chaque motif | Tous les jobs, avec `reference.rs` |
| `scenario` | L'exécution d'un playbook : le code de sortie et toute la sortie standard | Tous les jobs, avec `reference.rs` |
| `shadow` | Un run d'un scénario choisi à travers le callback `volant.compat.record`, compressé dans `crates/volant/tests/golden/shadow` ; l'exemple du callback est aussi vérifié sous chaque ansible-core qu'il prend en charge (`just compat-matrix`) | Tous les jobs, avec `shadow_golden.rs`, qui rejoue chaque enregistrement et n'attend aucune divergence |
| `module` | Ce que chaque module natif a répondu sur une vraie machine | Le job `ssh` et `just test-reference`, avec `reference_modules.rs` |
| `galaxy` | Une commande `ansible-galaxy` lancée contre un faux serveur Galaxy (`golden/reference/galaxy_server.py`) : le code de sortie, les sorties standard et d'erreur, les requêtes vues par le serveur, et les fichiers laissés avec leur SHA-256, dans `crates/volant/tests/golden/galaxy/<case>/expected.json` | Tous les jobs, avec `galaxy_golden.rs`, qui demande Python 3 pour le serveur |
| `command-names` | La sortie des scripts de `tests/command-names`, qui appellent `ansible`, `ansible-playbook`, `ansible-vault`, `ansible-inventory`, `ansible-doc`, `ansible-config`, `ansible-galaxy`, `ansible-pull` et `ansible-console` | Le job `command-names` |

Rien dans un enregistrement n'est modifié à la main. Changez l'entrée, lancez `just golden <kind>` avec ansible-core 2.19.12 installé, et lisez le diff. Le lancer sur un arbre inchangé laisse les fichiers exactement comme ils étaient. Le type `module` a aussi besoin de `sudo`, de systemd, d'apt et de cron, parce qu'il crée un compte et une unité sur la machine.

Un cas `galaxy` est un `case.yml` qui contient `args`, les arguments après `ansible-galaxy`, où `<golden-server>` tient la place de l'URL du serveur et `<golden-tmp>` celle du répertoire de travail du run. Le serveur construit lui-même ses collections et ses rôles, avec des dates et des propriétaires fixes, pour qu'une archive ait le même SHA-256 partout. Dans un enregistrement, le répertoire de travail se lit `<golden-tmp>` et le port `<golden-port>`. Tout autre fichier du répertoire du cas est copié dans le répertoire de travail, où la commande tourne, pour que `-r requirements.yml` puisse en nommer un. `galaxy_golden.rs` rejoue chaque requête enregistrée contre le serveur, et lance `volant galaxy` sur chaque cas dont la sous-commande est native.

## Le job command-names

Le job `command-names` s'exécute dans un conteneur `debian:stable-slim` sans `ansible` ni Python. Il lie les noms `ansible-*` au `volant` statique avec `volant shims install`, puis lance les scripts de `tests/command-names` exactement comme un utilisateur les écrirait pour ansible. Chaque script écrit sa sortie dans un fichier et la compare à `tests/command-names/expected/<script>.out`, un enregistrement d'ansible-core 2.19.12 fait par `just golden command-names`. Le job téléverse les fichiers de sortie quand un script échoue.

## Ajouter un scénario

Un scénario est un répertoire sous `crates/volant/tests/scenarios` :

1. Créez `crates/volant/tests/scenarios/<name>/` et mettez-y le playbook, l'inventaire et tous les fichiers que le playbook lit.
2. Écrivez `case.yml` à côté. Une seule exécution s'écrit `args: [-i, inv.ini, site.yml]`. Plusieurs exécutions du même répertoire s'écrivent `runs:`, une liste de `{args: [...]}`.
3. Lancez `just golden scenario`. Cela écrit `expected.json` dans le répertoire.
4. Lancez `touch crates/volant/tests/reference.rs` puis les tests. La liste des scénarios est fixée quand ce fichier est compilé : un nouveau répertoire n'est donc pas vu avant une recompilation.

Chaque exécution de Volant a lieu dans une copie neuve du répertoire, avec `--forks 1` sauf si le cas dit autre chose. Le test compare le code de sortie et toute la sortie standard, durées masquées. Outre `args` et `runs`, `case.yml` accepte six autres clés :

- `modes` liste `check`, `check-diff` et `diff`. Chaque exécution a alors lieu une fois de plus par mode, avec `--check`, `--check --diff` ou `--diff` ajouté, dans cet ordre fixe, et chaque exécution est enregistrée.
- `env` définit des variables d'environnement pour l'exécution.
- `program: ansible` enregistre les exécutions avec `ansible` au lieu d'`ansible-playbook`, et lance `volant adhoc` au lieu de `volant playbook`.
- `forks` relève le nombre de forks au-dessus de un. Les hôtes peuvent alors afficher dans un ordre différent : listez donc les lignes dont l'ordre peut différer dans `unordered`, sous forme d'expressions régulières. `unordered` sans `forks` est refusé.
- `needs` nomme ce que la machine doit avoir : `ansible-core`, `sudo` ou `systemd`. Un cas dont les besoins ne sont pas satisfaits est ignoré, sauf quand `VOLANT_PYTHON` ou `VOLANT_REQUIRE_REFERENCE=1` est défini, ce qui transforme l'omission en échec : nextest n'affiche pas l'omission d'un test qui passe, si bien qu'une machine sans ansible-core serait au vert. Les recettes `ssh-test` définissent `VOLANT_REQUIRE_REFERENCE=1`.
- `links` associe un chemin à la cible d'un lien que chaque copie reçoit à ce chemin. Utilisez-le pour un lien que git ou cargo ne peuvent pas contenir, comme un lien vers un fichier qui n'existe pas : `cargo package` refuse un lien suivi de ce genre.

## La matrice de parité

`crates/volant/tests/parity.rs` compare chaque nom que connaissent ansible-core 2.19.12 et les collections du paquet `ansible` fixé (mots-clés, modules, filtres, tests, lookups et les autres types de plugins) avec ce que le moteur en fait. `just parity-reference` régénère le côté référence, `crates/volant/tests/parity/reference.json`. Les pages [Parité](/fr/docs/reference/parity/), [Mots-clés](/fr/docs/reference/keywords/) et [Modules](/fr/docs/reference/modules/) sont générées à partir des tables propres du moteur par `just docs-parity`, `just docs-keywords` et `just docs-modules`, et un test échoue quand l'une d'elles est périmée.

## Le corpus YAML

`just yaml-corpus` charge chaque fichier YAML d'un corpus épinglé avec le chargeur de Volant et avec celui d'ansible-core, et compare les valeurs. Le corpus réunit les collections du paquet `ansible` épinglé, l'arborescence `test/` d'ansible-core et des dépôts publics aux commits que nomme `crates/volant/tests/parity/yaml_corpus.yml` : kubespray, debops, ceph-ansible, openstack-ansible et d'autres, environ onze mille fichiers. L'exemple `yaml_dump` affiche le côté de Volant, sans les encodages propres à Volant, et `crates/volant/tests/parity/yaml_corpus.py` affiche les fichiers qui diffèrent, regroupés par cause. Volant lit une date comme du texte, et cet écart n'est pas compté.

`crates/volant/tests/parity/yaml_corpus_known.txt` liste chaque écart connu avec l'issue qui le suit. La recette échoue sur tout autre écart. Le workflow `YAML corpus` la lance chaque semaine et à chaque changement du chargeur. Retirez une entrée quand son issue est corrigée.

## Les lancer

`just check-touched` est la vérification à lancer avant une pull request. Elle formate, lance clippy et le cliquet de taille de fichier, lance les tests des modules que la branche change, et vérifie l'orthographe. `just check-local` fait la même chose avec tous les tests. La CI lance tous les tests.

`just test` lance tout ce qui n'a besoin d'aucun hôte. `just test-reference` lance les enregistrements de modules contre la machine sur laquelle vous êtes, ce qui demande ansible-core, sudo et systemd. `just ssh-test` est ce que lance le job `ssh` : il part d'un sshd sur localhost et couvre aussi les tests de bout en bout. Préférez un scénario ou une ligne de table dans un test existant à une nouvelle fonction de test.
