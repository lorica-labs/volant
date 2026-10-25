---
sourceHash: 9e8e2288a96a
title: "Choisir la version d'ansible-core par projet, et garder ce qu'Ansible retire"
description: "Fiche de décision 0010."
sidebar:
  label: "0010 · Profils de version"
  order: 10
---

| Statut | Date |
|---|---|
| proposé | 2026-10-25 |

## Contexte

Volant reproduit ansible-core 2.19.12 ([décision 0002](/fr/docs/decisions/0002-ansible-core-2-19-as-reference/)). Ansible continue d'avancer : 2.20 et 2.21 sont sorties et 2.22 est en release candidate. Chaque version retire ou modifie des comportements dont dépendent des playbooks. Certaines équipes restent sur la version qu'elles utilisent aujourd'hui, d'autres veulent le comportement le plus récent, et une référence unique ne peut pas servir les deux.

Le 2026-10-25, le corpus golden a été réenregistré sous ansible-core 2.20.10, 2.21.5 et 2.22.0rc1, puis comparé aux enregistrements de 2.19.12 après retrait des heures, des identifiants de job et des identifiants de processus. Le corpus couvre les templates, les inventaires et 355 scénarios exécutés 1032 fois. Quatorze fichiers diffèrent sous 2.20, 76 sous 2.21 et 94 sous 2.22. La plupart des changements sont petits, comme une clé ajoutée à un résultat ou un nouveau préfixe sur une erreur, mais certains changent un run : un résultat de boucle n'a plus `skipped`, donc un playbook qui lit `r.skipped` s'arrête avec le code de sortie 2 sous 2.21. L'[annexe](#annexe--diff-du-corpus-2026-10-25) les liste.

## Décision

### Les profils

Un profil est la version d'ansible-core dont Volant reproduit le comportement. Il en existe deux : `2.19`, le profil par défaut, qui reste ce que Volant fait aujourd'hui, et `2.22`, ajouté une fois ansible-core 2.22.0 sorti et son corpus enregistré. `latest` désigne le profil le plus récent que Volant embarque.

À partir de 2.22, un profil est ajouté pour chaque version d'ansible-core, dans la version de Volant qui livre ses enregistrements, et `latest` passe à ce profil. Un profil est retiré quand ansible-core cesse de maintenir sa version. Les versions entre 2.19 et 2.22 n'ont pas de profil propre : leurs changements font partie de `2.22`.

Un projet choisit son profil avec `[volant] core_version` dans son `ansible.cfg` ou avec la variable d'environnement `VOLANT_CORE_VERSION`, qui l'emporte sur le fichier. La valeur est une version (`2.19`, `2.22`) ou `latest`. Toute autre valeur arrête le run avec le code de sortie 5 et liste les profils. La façon dont `volant.lock` enregistre le profil est décidée avec le verrou lui-même, dans #653.

`ansible_version` donne la version du profil : 2.19.12 sous `2.19`, et sous `2.22` la version corrective avec laquelle le corpus de `2.22` a été enregistré. Cela étend la [décision 0003](/fr/docs/decisions/0003-ansible-version-reports-the-reference-release/). `volant_version` ne change pas.

Les modules Python viennent de l'ansible-core installé sur le contrôleur ([décision 0006](/fr/docs/decisions/0006-warm-python-path/)), donc leurs résultats suivent cette version quel que soit le profil. Volant avertit au début du run quand la version installée diffère du profil.

### Des interrupteurs par comportement

Il n'y a qu'un moteur. Chaque différence entre versions est un interrupteur nommé, avec la version où il commence, par exemple `loop-results-omit-skipped` à partir de 2.21, et le code demande au profil si l'interrupteur est actif. Chaque interrupteur a une entrée du même nom dans le registre ci-dessous.

Des jeux de comportements séparés ont été écartés : les différences mesurées sont petites et dispersées dans le moteur, donc une copie par version dupliquerait presque tout pour changer une clé ici et un message là.

Le code des modules et des action plugins n'est jamais dupliqué. Un module natif ou un action plugin a une seule implémentation, et une différence entre versions est un interrupteur à l'intérieur. Un interrupteur est testé par un scénario enregistré sous chaque version qu'il change, et le test golden exécute chaque scénario sous chaque profil contre l'enregistrement de ce profil.

### Le corpus par profil

`just golden` prend la version en argument et enregistre avec un ansible-core de cette version, installé dans un environnement `uv` dédié. Les enregistrements de `2.19` restent où ils sont. Un autre profil écrit `expected.<version>.json` à côté de `expected.json`, seulement pour les scénarios dont le résultat diffère : 94 sur 355 sous 2.22.

Le Python du contrôleur est fixé avec la version. La mesure a montré qu'il change lui aussi les résultats : avec les mêmes templates de 2.19, Python 3.12 et 3.13 ne répondent pas pareil à `'abcabc'.replace('b', 'X', count=1)`, à de mauvais arguments passés aux méthodes de chaîne, ni au tri d'une liste qui contient `nan`.

L'enregistrement prend environ 52 minutes par version pour les goldens de templates, d'inventaires et de scénarios, sur une machine à 4 cœurs. Un mainteneur le fait une fois, comme aujourd'hui, et la CI n'enregistre jamais. La CI compare Volant aux enregistrements versionnés, ce qui coûte une exécution de plus des tests golden par profil.

Un comportement n'entre dans un profil qu'avec un enregistrement fait sous cette version.

### Le registre

Le registre liste ce qu'Ansible a retiré ou modifié, dans `crates/volant/src/profile/registry.toml`. Chaque entrée a un `name` (l'interrupteur), la fonctionnalité (`feature`), la version où elle a été retirée ou modifiée (`removed_in`), son type (`kind`, `security` ou `compat`), le remplacement (`replacement`), l'avertissement (`warning`) et, pour un bug connu en amont, le comportement que Volant suit (`follow`, `release` ou `documented`).

```toml
[[change]]
name = "debug-shows-registered-secret"
feature = "showing a registered secret with `debug`"
removed_in = "2.22"
kind = "security"
replacement = "write the value to a file with `copy` and `no_log: true`"
warning = "showing a registered secret with debug was removed in ansible-core 2.22"

[[change]]
name = "interpreter-auto-legacy"
feature = "`interpreter_python = auto_legacy` and `auto_legacy_silent`"
removed_in = "2.21"
kind = "compat"
replacement = "`auto` or `auto_silent`"
warning = "interpreter_python = auto_legacy was removed in ansible-core 2.21"

[[change]]
name = "loop-results-omit-skipped"
feature = "`skipped: false` in each loop result"
removed_in = "2.21"
kind = "compat"
replacement = "`r.skipped | default(false)`"
warning = "loop results lost their skipped key in ansible-core 2.21"
```

Sous un profil égal ou postérieur à `removed_in`, une fonctionnalité `compat` continue de marcher, avec l'avertissement suivi de « Volant still runs it » et du remplacement. Une fonctionnalité `security` reste désactivée sauf si `[volant] allow_insecure_legacy = true`. Sous un profil plus ancien, la fonctionnalité est simplement là, sans avertissement.

Une entrée est `security` quand les notes de version relient le changement à une CVE, quand le comportement retiré peut afficher un secret, ou quand il permet au contenu d'un playbook de lire ou d'écrire en dehors de ce qu'on lui a donné. Un correctif de sécurité qu'ansible-core livre dans une version corrective de 2.19 postérieure à 2.19.12 s'applique aussi au profil `2.19`. Les entrées `security` connues sont :

- `debug` qui affiche un secret enregistré, et les options `no_log` remplacées par un texte de substitution dans les résultats, tous deux modifiés en 2.22 ;
- le masquage des secrets par `$REDACTED$` dans toute la sortie et dans `log_path`, 2.22 ;
- `tempfile` qui accepte un chemin dans `prefix` et `suffix`, retiré en 2.19.13, 2.20.9 et 2.21.4 ;
- les identifiants affichés dans les URL par `uri`, `get_url`, `apt_key`, `rpm_key` et le lookup `url`, masqués depuis 2.19.13, 2.20.9 et 2.21.4 ;
- le répertoire persistant d'ansible-connection sans permissions privées, corrigé en 2.19.14, 2.20.10 et 2.21.5 ;
- `ansible-galaxy collection install` qui accepte des entrées de `FILES.json` hors de la collection (CVE-2026-89091), 2.22.0rc1.

Les autres retraits des guides de portage sont `compat` : entre autres `DEFAULT_TRANSPORT=smart` et le paramètre `vaultid` des filtres `vault` (2.20), le connection plugin `paramiko` et l'alias `gpg_whitelist` de `git` (2.21), et `yes` et `no` pour `follow_redirects` (2.22).

### `volant upgrade-plan`

```sh
volant upgrade-plan [PATH...] [--to PROFILE] [--json]
```

La commande lit les playbooks, les rôles, les inventaires et le `ansible.cfg` sous `PATH` (le répertoire courant par défaut) et liste chaque usage d'une fonctionnalité que le profil cible (`latest` par défaut) retire ou modifie par rapport au profil en vigueur. Chaque constat donne le `fichier:ligne`, l'entrée du registre et la modification suggérée.

Certains changements ne se trouvent pas à la lecture, comme un résultat lu par un nom calculé ou le texte d'une erreur. Le rapport se termine par les entrées du registre entre les deux profils qu'il ne peut pas vérifier. Pour celles-là, un projet enregistre un run sous sa version actuelle et le rejoue sous le profil cible avec [`volant shadow replay`](/fr/docs/start/switch-without-risk/). Les divergences sont les changements que le run rencontrerait.

Les codes de sortie sont 0 quand rien n'a été trouvé, 1 quand au moins un usage a été trouvé, 4 quand un fichier ne peut pas être lu et 5 pour un profil inconnu. `--json` donne un objet par constat : `path`, `line`, `name`, `removed_in`, `kind` et `suggestion`.

### Régressions connues

Volant suit la version quand un changement est voulu. Il suit le comportement documenté quand celui de la version est un bug ouvert en amont qui exécute la mauvaise chose sur un hôte. La réponse se décide au cas par cas et va dans le champ `follow` du registre. Pour les régressions connues de 2.21 :

- la découverte d'interpréteur qui réutilise le chemin d'interpréteur d'un autre hôte ([ansible/ansible#87288](https://github.com/ansible/ansible/issues/87288)) : `documented`, chaque hôte a sa propre découverte ;
- l'avertissement de découverte qui nomme l'hôte de l'inventaire au lieu de l'hôte délégué ([ansible/ansible#87298](https://github.com/ansible/ansible/issues/87298)) : `documented` ;
- la découverte non mise en cache pour les hôtes délégués ([ansible/ansible#86517](https://github.com/ansible/ansible/issues/86517)) : `documented`, puisque seul le temps diffère ;
- `dnf` qui échoue quand un plugin dnf écrit sur stdout ([ansible/ansible#87366](https://github.com/ansible/ansible/issues/87366)) : `release`. Le module s'exécute depuis l'ansible-core du contrôleur et Volant ne corrige pas les modules. Aucun changement de la sortie d'un `dnf` réussi n'a été trouvé dans le changelog ni dans le suivi des issues.

### Le verrou

`volant.lock` fixe le moteur qui exécute un projet. S'il enregistre le profil, et ce que fait un run quand le verrou et `core_version` ne s'accordent pas, se décide dans #653.

## Conséquences

- Un projet qui ne règle rien garde le comportement de `2.19`.
- Un projet qui passe à `latest` garde ses fonctionnalités retirées, avec un avertissement, sauf celles de sécurité.
- Chaque différence coûte un interrupteur, une entrée de registre et un enregistrement avant d'entrer dans un profil.
- L'implémentation suit dans ces issues, par taille :
  - le réglage du profil, `VOLANT_CORE_VERSION`, `ansible_version` par profil et la vérification de l'ansible-core du contrôleur (moyenne) ;
  - l'enregistrement par profil : l'argument de version de `just golden`, `expected.<version>.json` et les tests golden par profil (moyenne) ;
  - le registre, ses avertissements et `allow_insecure_legacy` (moyenne) ;
  - les interrupteurs du profil `2.22`, d'après l'annexe ci-dessous (grande, découpée par domaine une fois le réglage en place) ;
  - `volant upgrade-plan` (moyenne).

## Annexe : diff du corpus, 2026-10-25

ansible-core 2.20.10, 2.21.5 et 2.22.0rc1 face à 2.19.12. Les trois premières ont tourné sous Python 3.12, 2.22.0rc1 sous Python 3.13. Écartés : l'ordre des hôtes sous `order: shuffle` et sous `throttle`, qui varie d'un run à l'autre ; les chemins des fichiers de job et les identifiants de processus ; `ansible_version` lui-même ; et les différences de templates venues de Python plutôt que d'ansible-core.

| Fonctionnalité | 2.19.12 | 2.20 | 2.21 | 2.22 | Type |
|---|---|---|---|---|---|
| Test `regex` avec un `match_type` inconnu | `'str' object is not callable` | `Invalid match_type specified` | comme 2.20 | comme 2.20 | template |
| `password_hash` avec un sel donné | utilise le sel (`$1$saltsalt$…`) | comme 2.19 | un autre sel (`$1$n34PoBLM$…`) | comme 2.21 | template |
| Erreur dans un `when` | `Error while evaluating conditional: …` | comme 2.19 | `A 'when' expression failed: …` | comme 2.21 | template |
| `msg` d'une commande en échec | `non-zero return code` | comme 2.19 | `The command exited with a non-zero return code.` | comme 2.21 | module |
| Erreurs d'`include_vars` | chemin sans guillemets | chemin entre guillemets | comme 2.20 | comme 2.20 | module |
| Résultat de `copy` en check mode | pas de clé `file` | clé `file` | comme 2.20 | comme 2.20 | module |
| `command` dont le `chdir` échoue | `rc: null` | comme 2.19 | pas de clé `rc` | comme 2.21 | module |
| `cmd` d'une commande introuvable | une chaîne | comme 2.19 | comme 2.19 | une liste | module |
| `ansible_parsed` dans les résultats de module | absent | absent | absent | `true` | module |
| `skipped: false` dans les résultats de boucle | présent | présent | absent | absent | scénario |
| `changed_when_result` | absent | absent | présent | présent | scénario |
| Message de `failed_when` | `Action failed: Unknown error.` ou le nom de l'action | comme 2.19 | `A 'failed_when' expression evaluated to 'True'.` | comme 2.21 | scénario |
| Ligne de réessai d'`until` et `attempts` | `(1 retries left)`, `attempts: 1` | comme 2.19 | `(0 retries left)`, `attempts: 2` | comme 2.21 | scénario |
| Éléments de boucle qui lisent un fact posé dans la même boucle | `a-1`, `b-2`, `c-3` | comme 2.19 | `a-0`, `b-1`, `c-2` | comme 2.21 | scénario |
| `any_errors_fatal: "{{ false }}"` après un échec | l'autre hôte s'arrête | comme 2.19 | l'autre hôte continue | comme 2.21 | scénario |
| Fichier manquant dans `include_tasks` | clés `include` et `reason` | comme 2.19 | `msg` et une ligne `[ERROR]` | comme 2.21 | scénario |
| Source de `copy` manquante | message sur une ligne | comme 2.19 | `<<< caused by >>>` et les chemins cherchés | comme 2.21 | scénario |
| Fichier de rôle hors du rôle | code de sortie 4 | code de sortie 4 | code de sortie 1 | code de sortie 1 | scénario |
| `debug` d'un `msg` non défini | `{"msg": null}` | comme 2.19 | `{}` | comme 2.21 | scénario |
| `--start-at-task` avec une spec d'arguments de rôle | validation sautée | validation exécutée | comme 2.20 | comme 2.20 | scénario |
| `ansible --help` | `-K` seul | `-K \| --become-password-file` | comme 2.20 | comme 2.20 | scénario |
| Valeurs secrètes dans la sortie | affichées | affichées | affichées | `$REDACTED$`, même dans un chemin | scénario |
| Valeur chiffrée par vault dans `ansible-inventory --graph` | affichée | affichée | affichée | `$REDACTED$` | inventaire |

Sous 2.22, le masquage a aussi caché des parties de la sortie qui ne sont pas des secrets : le mot de passe become `wrong` a transformé le répertoire `become-su-local-wrong-password` en `become-su-local-$REDACTED$-password`.
