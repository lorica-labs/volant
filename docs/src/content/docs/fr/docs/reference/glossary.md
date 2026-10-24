---
sourceHash: 725d6c347d6e
title: Glossaire
description: Les mots que cette documentation emploie dans un sens précis.
---

### Plugin d'action

Code d'ansible-core qui s'exécute sur le contrôleur et décide ce qu'il faut envoyer à un hôte pour un module comme `copy` ou `package`. Volant en exécute huit en Rust et le reste par le pont. Voir [Plugins d'action](/fr/docs/internals/action-plugins/).

### Agent

Un petit programme statique que Volant téléverse sur chaque hôte géré et y garde en cache. Il reçoit les tâches du contrôleur, les exécute et renvoie les résultats en flux. Voir [Connexions et become](/fr/docs/hosts/connections/).

### Lot

Les tâches qu'un hôte exécute à la suite avant d'attendre de nouveau les autres. Par défaut, un lot a la largeur d'une tâche. Voir [Traitement par lots entre hôtes](/fr/docs/hosts/batching/).

### Pont

Le chemin qu'emprunte un plugin d'action quand Volant n'en a pas de version Rust : le Python propre du plugin, issu d'ansible-core, s'exécute dans l'assistant Python du contrôleur, et le contrôleur répond à ce qu'il demande. Voir [Sur le pont](/fr/docs/internals/action-plugins/#sur-le-pont).

### Contrôleur

La machine sur laquelle vous lancez `volant`, et le programme `volant` lui-même. Il lit les playbooks, rend les modèles et pilote les agents.

### Point de flush

Une étape où les handlers notifiés s'exécutent. Chaque play en a trois, et `meta: flush_handlers` en ajoute. Voir [Handlers](/fr/docs/playbooks/blocks/#handlers).

### Lien

Une connexion ouverte du contrôleur vers un agent. Un hôte a son propre lien, plus un pendant qu'il élève ses privilèges et un par hôte délégué.

### Runtime géré

Le Python et l'ansible-core que Volant installe pour lui-même et utilise avant tout interpréteur du système. Voir [Le runtime géré](/fr/docs/reference/runtime/).

### Module natif

Une version Rust d'un module que l'agent embarque. Il répond lui-même à une tâche quand ses arguments restent dans ce qu'il implémente, et rend la tâche au module Python sinon. Voir [Modules natifs](/fr/docs/internals/python/#modules-natifs).

### Vérification préalable

Le contrôle qui nomme, avant toute connexion, tout ce que cette version ne prend pas encore en charge, et arrête l'exécution. Voir [Ce qui n'est pas encore pris en charge](/fr/docs/internals/preflight/).

### La référence

ansible-core 2.19.12, la version dont Volant reproduit le comportement. Un écart avec elle est un bug, sauf si une page le liste comme voulu. Voir la [fiche de décision 0002](/fr/docs/decisions/0002-ansible-core-2-19-as-reference/).

### Point d'insertion

Une étape dont les suivantes ne sont connues qu'en cours d'exécution : un flush de handlers, ou un `include_tasks` ou `include_role` dynamique. Les nouvelles étapes y sont insérées. Voir [Includes et imports](/fr/docs/playbooks/roles/#imports-et-includes).

### Étape

Une entrée de la liste plate et numérotée en laquelle un play est compilé. Tous les hôtes parcourent la même liste. Voir [Exécution des plays](/fr/docs/playbooks/how-a-play-runs/).

### Point de synchronisation

Une étape où tous les hôtes d'un lot attendent les autres. Avec le comportement `linear` par défaut, chaque tâche en est un.

### Union

L'ensemble des modules qu'une exécution peut envoyer à un hôte, construit une fois avant la première connexion. Voir [Le chemin Python préchauffé](/fr/docs/internals/python/).

### Blob d'union

L'archive unique qui porte tous les modules Python dont une exécution a besoin, avec le code partagé qu'ils importent. Elle est envoyée une fois à chaque hôte. Voir [Le chemin Python préchauffé](/fr/docs/internals/python/).

### Serveur Python préchauffé

Un processus Python que l'agent garde en cours d'exécution sur un hôte. Il charge une fois le code de module partagé et crée un processus fils pour chaque tâche.
