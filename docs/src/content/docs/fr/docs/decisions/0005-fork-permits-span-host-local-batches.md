---
sourceHash: 71d56473bdd4
title: "Les jetons de fork couvrent les lots locaux à l'hôte"
description: "Fiche de décision 0005."
sidebar:
  label: "0005 · Jetons de fork et traitement par lots"
  order: 5
---

| Statut | Date |
|---|---|
| accepté, et soumis à une option depuis le 2026-09-20 (voir Statut plus bas) ; la borne sur les liens avec élévation est remplacée par la mise à jour du 2026-10-24 | 2026-09-16 |

## Contexte

Un pilote d'hôte prend l'un des jetons de `forks` avant d'ouvrir une connexion et le rend quand le lot qu'il a rassemblé a reçu sa réponse. La [fiche de décision 0004](/fr/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) laissait les connexions d'agent actives sans borne ; lier les liens avec élévation au jeton les borne, car `forks` limite alors les connexions ouvertes autant que les hôtes qui travaillent. Vingt hôtes qui passaient root sous `ulimit -n 128` et `-f 5` signalaient `Too many open files` pour un hôte parfaitement joignable.

Rendre les liens à chaque lot n'est pas gratuit, et un lot se termine plus souvent que le mot ne le laisse croire. Chacun de ces cas en ferme un : une tâche qui porte `register`, `loop`, `changed_when` ou `failed_when` ; une tâche qui lit l'état d'un autre hôte, ce qui est une frontière devant elle ; et une tâche qui passe à un autre utilisateur. Enregistrer un résultat est ordinaire : un play de six tâches avec élévation, chacune avec un `register`, payait donc six élévations, chacune avec une sonde `sudo` et un `sudo` qui démarre l'agent. Mesuré sur la machine de développement avec un `sudo` qui enregistre ce qu'on lui demande d'exécuter : douze appels pour ces six tâches.

Rien dans un `register` n'a besoin d'un autre hôte : le jeton était donc rendu devant une attente qui n'existait pas.

## Décision

Un pilote garde son jeton de fork et ses liens avec élévation d'un lot au suivant quand il s'apprête à enchaîner directement sur une étape qui lui est propre. Il rend les deux devant chaque attente des autres hôtes, et à la fin du play, où les garder ne ferait qu'occuper des descripteurs. Un pilote qui quitte l'exécution par un échec les rend aussi.

Quatre étapes comptent comme une attente : celle que la table des mots-clés déclare comme point de synchronisation, celle dont le texte nomme l'état d'un autre hôte, un point d'insertion où la liste des étapes grandit, et une étape dont le successeur se trouve derrière l'un d'eux. Il en va de même d'une étape retenue jusqu'à ce que son propre résultat décide où va l'hôte.

Les liens avec élévation sont fermés hors tâche plutôt qu'attendus, comme avant : `shutdown` accorde à son agent jusqu'à deux secondes, et un pilote qui les paierait entre deux lots sérialiserait ce que la libération du jeton vient de libérer.

Cette décision n'ajoute aucun multiplexage de connexions. Chaque lien reste son propre processus `ssh`.

## Conséquences

- Les liens actifs sont bornés à un par hôte, pour la connexion qui survit au play, plus `forks` fois le nombre d'utilisateurs cibles distincts vers lesquels un pilote passe pendant qu'il tient un jeton. La borne précédente comptait un lien avec élévation par pilote parce que chaque lot fermait le sien ; plusieurs lots consécutifs sous un même jeton peuvent maintenant nommer des utilisateurs différents.
- Mesuré, vingt hôtes à `-f 5` passant root : le contrôleur culmine à 84 descripteurs ouverts là où il culminait à 69 avant, pour une limite de 128. L'exécution se termine avec le code 0 sans hôte injoignable, ce qui est la preuve sur laquelle la borne précédente avait été acceptée.
- Un play de six tâches avec élévation, chacune fermant son lot par un `register`, démarre un agent avec élévation au lieu de six.
- Les hôtes passent à tour de rôle à une attente plutôt qu'à chaque lot. Avec `forks` inférieur au nombre d'hôtes actifs et un play dont toutes les étapes sont locales à l'hôte, un hôte exécute maintenant ses étapes restantes avant que le suivant démarre, là où les deux alternaient auparavant. Le travail est le même et le récapitulatif aussi ; ce qui change est l'ordre d'arrivée des lignes, qui était déjà sans ordre entre deux hôtes que rien ne synchronisait.
- Un play qui n'élève jamais les privilèges n'est touché qu'en gardant son jeton un peu plus longtemps, et un play dont chaque tâche lit d'un hôte à l'autre rend le jeton à chaque étape, comme avant.

Remplace la conséquence « les connexions d'agent actives par hôte ne sont pas bornées » de la [fiche de décision 0004](/fr/docs/decisions/0004-become-runs-a-second-agent-under-sudo/).

## Statut

Depuis le 2026-09-20, un pilote ne garde son jeton au-delà d'une étape locale à l'hôte que si l'opérateur le demande. `[volant] batching = true` dans `ansible.cfg`, ou `VOLANT_BATCHING=1`, active cette décision ; sans cela, chaque étape est une attente, comme `linear` dit qu'elle l'est.

La raison est la quatrième conséquence ci-dessus. Les étapes qui comptent comme une attente sont lues dans le texte d'une tâche, et une dépendance qui passe par un fichier, une base de données ou un service ne laisse aucun texte à lire. Le `linear` d'Ansible promet que les hôtes se retrouvent devant chaque tâche, et un playbook écrit sur cette promesse a le droit de s'y fier. En décider autrement d'après le seul texte était le mauvais choix par défaut pour un moteur dont la première promesse est que les playbooks s'exécutent sans modification.

Tout ce que décrit la décision est conservé et s'exécute toujours sous l'option. Le coût mesuré des deux réglages, et ce que le défaut garde dans les deux cas, se trouvent dans [Traitement par lots entre hôtes](/fr/docs/hosts/batching/).

## Mise à jour, 2026-10-24

La première conséquence ci-dessus ne tient plus. Depuis le 2026-09-22 ([#178](https://github.com/lorica-labs/volant/pull/178)), un hôte garde son lien avec élévation à travers un point de synchronisation, et il tient son propre lien plus au plus un lien avec élévation, quel que soit le nombre d'utilisateurs cibles distincts. Un lot qui élève différemment de celui en cours (un autre utilisateur, un autre programme d'élévation ou d'autres options, un autre transport) ferme l'ancien lien et en ouvre un nouveau. Le jeton lui-même est toujours rendu devant chaque attente, comme décidé ici. [Connexions et become](/fr/docs/hosts/connections/#connexions-persistantes) énonce la règle actuelle.
