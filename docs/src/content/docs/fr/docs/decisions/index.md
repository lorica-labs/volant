---
sourceHash: 15cf72956526
title: Fiches de décision
description: Les décisions qui déterminent ce que voient les utilisateurs de Volant, et pourquoi elles ont été prises.
sidebar:
  label: À propos des fiches de décision
  order: 0
---

Une fiche de décision explique un choix qui touche les utilisateurs de Volant : quelle était la situation, ce qui a été décidé et ce qui en découle. Les fiches suivent le format [MADR](https://adr.github.io/madr/) et sont numérotées dans l'ordre où les décisions ont été prises.

Une fiche n'est pas réécrite une fois acceptée. Quand une décision ultérieure la modifie, la ligne de statut le dit et renvoie vers la nouvelle fiche. Les mesures prises après la décision sont ajoutées sous forme de sections datées à la fin.

| Fiche | Décision |
|---|---|
| [0001](/fr/docs/decisions/0001-gpl-3-0-or-later-license/) | Placer le dépôt sous licence GPL-3.0-or-later. Les crates d'interface font exception, voir 0009. |
| [0002](/fr/docs/decisions/0002-ansible-core-2-19-as-reference/) | Reproduire ansible-core 2.19, et traiter toute différence comme un bug. |
| [0003](/fr/docs/decisions/0003-ansible-version-reports-the-reference-release/) | `ansible_version` rapporte la version de référence ; `volant_version` rapporte celle de Volant. |
| [0004](/fr/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) | `become` démarre un second agent sous `sudo`. |
| [0005](/fr/docs/decisions/0005-fork-permits-span-host-local-batches/) | Un hôte garde son jeton de fork pendant les étapes locales à l'hôte, derrière l'option `batching`. |
| [0006](/fr/docs/decisions/0006-warm-python-path/) | Envoyer un blob d'union des modules Python par exécution, et les forker depuis un serveur préchauffé. |
| [0007](/fr/docs/decisions/0007-action-plugins/) | Exécuter les plugins d'action comme une suite de sous-tâches pilotée par le contrôleur. |
| [0008](/fr/docs/decisions/0008-native-modules-and-speed/) | Répondre aux modules courants nativement dans l'agent, confier le reste à Python, et partager une connexion ssh par hôte. |
| [0009](/fr/docs/decisions/0009-interface-crates-mit-or-apache/) | Garder le moteur sous GPL-3.0-or-later, et placer sous MIT OR Apache-2.0 les crates qui décrivent une interface publique. |
| [0010](/fr/docs/decisions/0010-core-version-profiles/) | Laisser chaque projet choisir la version d'ansible-core que Volant reproduit (`2.19` ou `latest`), garder avec un avertissement ce qu'Ansible a retiré, et lister ce que change une montée de version (`volant upgrade-plan`). |
| [0011](/fr/docs/decisions/0011-extensions-and-eject/) | N'étendre Ansible que par des clés qu'une commande sait réécrire en Ansible simple (`volant eject`, `volant check --portable`). |
