---
sourceHash: 65b041000adc
title: "`become` exécute un second agent sous `sudo`"
description: "Fiche de décision 0004."
sidebar:
  label: "0004 · become sous sudo"
  order: 4
---

| Statut | Date |
|---|---|
| accepté ; la borne sur les connexions actives est remplacée par la [fiche de décision 0005](/fr/docs/decisions/0005-fork-permits-span-host-local-batches/) et la liste des méthodes est étendue à `su` (voir la mise à jour du 2026-10-24) | 2026-09-09 |

## Contexte

Ansible élève les privilèges tâche par tâche, en enveloppant chaque appel de module dans `sudo`. Volant exécute les tâches par un agent de longue durée par hôte : envelopper chaque tâche reviendrait à avoir soit un agent qui appelle `sudo` pour chaque module, soit un agent lui-même privilégié pour tout le play.

## Décision

Le contrôleur ouvre une connexion d'agent par couple (hôte, utilisateur cible). Une tâche avec `become` passe par l'agent démarré par `sudo -H <form> -u <user> -- volant-agent`, où `<form>` vaut `-n` ou `-k -S -p ''` selon ce que décide la sonde décrite plus bas ; les tâches sans `become` gardent l'agent sans privilèges. Seul `sudo` est pris en charge ; les autres méthodes ne le sont pas encore et arrêtent l'exécution. L'agent lui-même ne sait rien de l'élévation.

## Conséquences

- Un changement d'utilisateur cible entre deux tâches consécutives termine le lot ; le cas courant, un play entier sous `become`, garde un seul lot.
- La forme de l'appel à `sudo` est fixée par une sonde, non par la présence d'un mot de passe. Un `sudo` qui n'exige aucune authentification ne lit pas son entrée standard : sur un hôte avec une authentification en cache ou une règle `NOPASSWD`, un mot de passe écrit là reste dans le tube et c'est l'*agent* qui le lit comme en-tête de sa première trame. Le contrôleur essaie donc d'abord `sudo -n` et ne se rabat sur `sudo -k -S -p ''` qu'une fois que `sudo` a lui-même rejeté la demande faute d'authentification. Deviner la forme d'après le seul mot de passe bloque le lien, et seulement à la seconde connexion avec élévation dans la fenêtre de l'horodatage, ce qui explique pourquoi la sonde n'est pas facultative.
- Le mot de passe, quand il y en a un, est écrit une fois sur l'entrée standard de `sudo` et n'apparaît jamais dans une ligne de commande, un journal ou un résultat. Les deux types qui le portent ont des implémentations de `Debug` écrites à la main qui affichent `<redacted>` : un vidage `-vvv` ne peut donc pas le divulguer, et la sortie d'erreur capturée de `sudo` en est purgée avant d'atteindre un message.
- Une élévation qui échoue est une tâche en échec, non un hôte injoignable : la connexion a fonctionné.
- Les connexions d'agent actives par hôte ne sont pas bornées. `forks` borne le nombre d'hôtes qui travaillent en même temps, non le nombre de liens qu'un hôte tient, et la table garde un lien par utilisateur cible distinct jusqu'au récapitulatif, chacun étant un processus `ssh` séparé. Un lien avec élévation coûte aussi une connexion `ssh` supplémentaire pour la sonde, deux quand un mot de passe est nécessaire. Cette décision n'ajoute aucun multiplexage de connexions : chaque lien est son propre processus `ssh`, et un large inventaire paie chacun séparément.
- `su`, `doas`, `pbrun` et les autres pourront être ajoutés comme autres façons de démarrer l'agent, sans changement de protocole.

## Mise à jour, 2026-10-24

`su` est pris en charge depuis le 2026-10-16 ([#333](https://github.com/lorica-labs/volant/pull/333)), et sur une connexion `local` depuis le 2026-10-23 ([#755](https://github.com/lorica-labs/volant/pull/755)). Il démarre l'agent comme `sudo`, ce qui est l'extension qu'attendait la dernière conséquence ci-dessus, et il n'exige aucun changement de protocole. `doas`, `pbrun` et les autres méthodes sont toujours refusés. [Become](/fr/docs/hosts/connections/#become) décrit les deux méthodes.

La borne sur les connexions actives a changé deux fois après cette fiche. La [fiche de décision 0005](/fr/docs/decisions/0005-fork-permits-span-host-local-batches/) l'a liée au jeton de fork. Depuis le 2026-09-22 ([#178](https://github.com/lorica-labs/volant/pull/178)), un hôte tient son propre lien plus au plus un lien avec élévation, quel que soit le nombre d'utilisateurs cibles du play. Une élévation vers un autre utilisateur remplace le lien avec élévation en cours.
