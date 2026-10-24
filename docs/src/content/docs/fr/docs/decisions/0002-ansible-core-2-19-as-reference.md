---
sourceHash: 833100c102c9
title: "Utiliser ansible-core 2.19 comme référence de compatibilité"
description: "Fiche de décision 0002."
sidebar:
  label: "0002 · Référence de compatibilité"
  order: 2
---

| Statut | Date |
|---|---|
| accepté | 2026-09-05 |

## Contexte

Volant est conçu pour exécuter sans modification les playbooks existants. « Sans modification » demande une définition : ansible-core change de comportement d'une version à l'autre, et la 2.19 a réécrit le templating (marquage des données) avec quelques ruptures voulues.

## Décision

Volant reproduit le comportement d'ansible-core 2.19. Les différences avec la 2.19 sont des bugs. Les tests golden et le banc de compatibilité comparent avec cette version. Un job nocturne contre la branche de développement d'ansible-core, pour voir venir les changements avant leur arrivée dans une version, est prévu mais n'a pas été construit : rien ne surveille aujourd'hui cette branche.

## Conséquences

- Les playbooks qui ne fonctionnaient sur des versions plus anciennes que grâce à un comportement supprimé dans la 2.19 sont hors périmètre.
- Déplacer la référence vers une version plus récente d'ansible-core est une nouvelle décision, consignée ici, avec un changement de version majeure ou mineure selon ce qui change.
