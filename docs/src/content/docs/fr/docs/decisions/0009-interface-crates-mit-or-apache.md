---
sourceHash: dad3d3f2ec02
title: "Garder le moteur sous GPL et placer les crates d'interface sous MIT OR Apache-2.0"
description: "Fiche de décision 0009."
sidebar:
  label: "0009 · Licences d'interface"
  order: 9
---

| Statut | Date |
|---|---|
| accepté | 2026-10-24 |

## Contexte

D'autres programmes veulent piloter Volant et lire ce qu'une exécution a fait : systèmes d'intégration continue, éditeurs, tableaux de bord et agents d'IA. Aujourd'hui, ils ne peuvent qu'analyser la sortie lisible par un humain, qui suit le format texte d'Ansible et est écrite pour des personnes. Un flux d'événements documenté est prévu, avec une petite crate, `volant-events`, qui contient ses types et son schéma JSON pour qu'un programme Rust n'ait pas à les redéfinir.

La [fiche de décision 0001](/fr/docs/decisions/0001-gpl-3-0-or-later-license/) a placé tout le dépôt sous GPL-3.0-or-later. Sous cette licence, tout programme qui lie `volant-events` devrait être lui aussi sous GPL, ce qui empêcherait la plupart des outils d'utiliser une interface destinée à tous.

## Décision

Le moteur reste sous GPL-3.0-or-later. Cela couvre `volant`, `volant-agent`, `volant-protocol` et tout le reste du dépôt, sauf les crates ci-dessous.

Une crate dont le seul but est de décrire une interface publique pour d'autres programmes est placée sous MIT OR Apache-2.0. La première sera `volant-events`.

Les autres programmes parlent à Volant par sa ligne de commande et son flux d'événements documenté. Ils ne lient pas les crates du moteur.

## Conséquences

- Le moteur reste un logiciel libre. Cette fiche est la promesse du projet de ne pas passer à une licence propriétaire ou à code source disponible.
- Un programme sous n'importe quelle licence peut lire les événements de Volant, par `volant-events` ou à partir du seul schéma JSON.
- Une crate d'interface définit `license = "MIT OR Apache-2.0"` dans son propre `Cargo.toml` au lieu d'hériter de la licence de l'espace de travail, et garde `LICENSE-MIT` et `LICENSE-APACHE` dans son répertoire. Le `LICENSE` à la racine couvre toujours le reste du dépôt.
- Les contributions à une crate d'interface sont acceptées sous MIT OR Apache-2.0, signées selon le Developer Certificate of Origin comme toute autre contribution.
- Une crate d'interface est écrite pour son but et ne reprend aucun code d'une crate GPL.
- `volant-protocol` reste sous GPL. La placer sous une autre licence exigerait l'accord de tous ceux qui y ont contribué.
