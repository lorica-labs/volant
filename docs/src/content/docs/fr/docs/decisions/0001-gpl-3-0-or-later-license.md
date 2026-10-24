---
sourceHash: 8cfc8ab189fd
title: "Utiliser GPL-3.0-or-later pour tout le dépôt"
description: "Fiche de décision 0001."
sidebar:
  label: "0001 · Licence"
  order: 1
---

| Statut | Date |
|---|---|
| accepté ; son périmètre est restreint par la [fiche de décision 0009](/fr/docs/decisions/0009-interface-crates-mit-or-apache/), qui place sous MIT OR Apache-2.0 les crates décrivant une interface publique | 2026-09-05 |

## Contexte

Volant est conçu pour exécuter des modules Ansible non modifiés. Pour cela, le contrôleur appellera le constructeur de charges utiles d'ansible-core par un interpréteur Python embarqué, et l'agent enverra les `module_utils` d'ansible-core aux hôtes gérés. ansible-core est sous licence GPL-3.0. Importer une bibliothèque GPL à l'exécution et en redistribuer des parties rend juridiquement fragile une licence permissive pour Volant.

Cette version n'en est pas encore là. Elle n'embarque aucun interpréteur Python, ne contient aucun code d'ansible-core et n'exécute que les modules natifs. La licence est choisie pour l'architecture vers laquelle le projet se dirige plutôt que pour celle qu'il a, car un dépôt dont tous les contributeurs ont signé sous une seule licence est difficile à faire passer sous une autre.

## Décision

Tout le dépôt est placé sous licence GPL-3.0-or-later, celle d'Ansible et de la plupart des collections.

## Conséquences

- Les contributeurs signent leurs commits selon le Developer Certificate of Origin.
- L'adoption n'en est pas affectée : Ansible lui-même est un outil en ligne de commande sous GPL.
- Quand le constructeur de charges utiles sera réécrit en Rust et que l'agent ne contiendra plus de code d'ansible-core, la crate de l'agent pourra passer sous Apache-2.0. Ce sera une nouvelle décision.

## Mise à jour, 2026-09-22

Le contexte ci-dessus décrit le projet au moment où cette décision a été prise. Depuis la [fiche de décision 0006](/fr/docs/decisions/0006-warm-python-path/), Volant envoie les `module_utils` d'ansible-core aux hôtes gérés, et le contrôleur appelle ansible-core par un processus auxiliaire plutôt que par un interpréteur embarqué. Le raisonnement sur la licence est inchangé.

## Mise à jour, 2026-10-24

La [fiche de décision 0009](/fr/docs/decisions/0009-interface-crates-mit-or-apache/) garde le moteur sous GPL-3.0-or-later et place sous MIT OR Apache-2.0 les crates qui ne font que décrire une interface publique. Elle garde aussi `volant-agent` et `volant-protocol` sous GPL-3.0-or-later : le changement de licence éventuel de la crate de l'agent, évoqué dans les conséquences ci-dessus, n'est donc pas prévu.
