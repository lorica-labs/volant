---
title: "Modules natifs avec repli sur Python, et les autres choix de vitesse"
description: "Fiche de décision 0008."
sidebar:
  label: "0008 · Modules natifs et vitesse"
  order: 8
---

| Statut | Date |
|---|---|
| accepté | 2026-09-27 |

## Contexte

Le 2026-09-24, Volant a exécuté les quatre rôles Galaxy de la [fiche de décision 0007](/fr/docs/decisions/0007-action-plugins/#résultats) contre deux hôtes convergés en 12.66 s, contre 26.23 s pour ansible-core 2.19.12 avec pipelining. Un profil par étape de cette exécution a montré où allait le temps, par hôte :

- 5.5 s dans huit tâches `apt` sans rien à installer : 140 ms pour importer le module et 606 ms pour l'exécuter, à chaque appel ;
- 3.8 s à reconstruire l'union des modules Python sur le contrôleur, alors que rien n'avait changé dans ansible-core depuis la dernière exécution ;
- 1.1 s dans `setup`, dont environ 300 ms pour démarrer le serveur Python, puis 0.7 s dans trois tâches `systemd` et 0.7 s dans `file`, `lineinfile` et `stat` ;
- 0.6 s à ouvrir les connexions, parce que la sonde et le lien payaient chacun leur propre négociation ssh ;
- 0.2 s à préparer les tâches sur le contrôleur. Sur k3s, la même étape prenait 2 s, et un profil CPU en plaçait environ 80 % dans la copie et la libération de tables de variables.

Les trames elles-mêmes prenaient 30 ms, et sérialiser toute une exécution environ 5 ms : le format de transmission n'était donc pas là où allait le temps.

Le même profil du playbook k3s-ansible a ajouté `service_facts` (3.3 s), `package_facts` (2.1 s) et une construction d'union de 5.7 s qui incluait des collections.

## Décision

Les modules courants s'exécutent nativement dans l'agent, avec un repli sur Python. `setup`, `stat`, `file`, `apt`, `systemd` (et `systemd_service`), `lineinfile`, `copy`, `package_facts`, `service_facts`, `user` et `group` ont une version Rust dans l'agent. Un module natif reçoit les arguments que le module Python aurait reçus. Il répond avec le résultat du module de référence quand ces arguments entrent dans ce qu'il implémente, et sinon il rend la tâche au module Python. Il décide en lisant, et n'agit qu'ensuite : un retour de main survient donc toujours avant que quoi que ce soit ait changé sur l'hôte. Son résultat entre dans le contrôleur par le même chemin que celui d'un module Python, comme donnée non fiable.

La comparaison avec les goldens est le critère d'acceptation : un module natif n'est activé que dans la modification où ses réponses correspondent à la référence clé pour clé, `changed`, `diff` et `invocation` compris, pour chaque cas qu'enregistre le golden. Le même test vérifie quel chemin chaque cas a pris : un module natif qui a répondu hors de son sous-ensemble, ou qui a rendu la main pour tous les cas, fait échouer ce test.

Les facts ne sont collectés nativement que lorsque le playbook ne démontre aucun besoin d'autre chose. Le `setup` natif produit les 17 collecteurs de `gather_subset: min`, les facts de processeur et de mémoire de `hardware`, et les routes par défaut et listes d'adresses de `network`. Un fact qu'il ne sait pas produire est absent du résultat. Avant de se connecter, le contrôleur lit chaque play, rôle, modèle et définition de variable de l'exécution et dresse la liste des facts qu'ils peuvent lire. Il n'utilise le `setup` natif que si chacun d'eux est produit nativement. Une lecture par un nom calculé (`ansible_facts[item]`, `vars[...]`, `lookup('vars', ...)`, un `hostvars[host]` entier) compte comme lire tout et garde le `setup` Python. `--facts python` le force quand même.

L'union est mise en cache sur le contrôleur : une entrée est conservée sous `$XDG_CACHE_HOME/volant/unions/`, retrouvée grâce à tout ce que la construction a lu et qui n'est pas un fichier : la version de Volant, l'interpréteur, les modules demandés, l'environnement `ANSIBLE_*` et `PYTHON*`, `ansible.cfg` et le répertoire de travail. Elle enregistre la taille et la date de modification de chaque fichier source. Vérifier les 154 fichiers coûte environ 1 ms. Tout changement reconstruit, et un cache illisible ou impossible à écrire ne fait jamais échouer une exécution.

Chaque hôte de l'inventaire obtient une connexion OpenSSH. Volant ajoute `ControlMaster`, un `ControlPath` à lui et `ControlPersist=30s` à sa ligne de commande `ssh`, sauf si la configuration ssh de l'opérateur ou `ansible_ssh_*_args` disent déjà quelque chose sur le partage de connexion. La sonde, la vérification de l'envoi, le lien et le lien avec élévation partagent alors un seul échange de clés. Le socket est par hôte d'inventaire plutôt que par machine : cinquante alias d'une même machine partageant une connexion maître se sont heurtés à la limite `MaxSessions` de 10 du serveur.

Les variables d'un hôte forment une pile de couches partagées lues de haut en bas, au lieu d'une table fusionnée et copiée pour chaque tâche. Les facts collectés sont conservés une fois et partagés par chaque tâche qui les lit, et un fact écrit par un hôte n'invalide que la vue de cet hôte.

La barrière reste, et le traitement par lots reste facultatif. Il voit maintenant aussi les lectures d'autres hôtes dans les corps de modèles et dans les définitions de variables, mais il reste désactivé par défaut. Il décide d'après le texte du playbook, et une dépendance que le texte n'explicite pas, comme un service qu'un autre hôte est encore en train d'installer, ne serait pas attendue. Voir [Traitement par lots entre hôtes](/fr/docs/hosts/batching/).

Les blobs sont binaires et compressés. La version 6 du protocole envoie l'union et les fichiers déposés sous forme d'octets bruts dans la trame qui suit l'annonce, et non plus en base64 dans du JSON. L'union est compressée avec deflate une fois, au moment où elle est écrite dans le cache du contrôleur. Un fichier déposé est compressé quand il dépasse 4 KiB et que la compression le raccourcit. L'agent décompresse jusqu'à la taille annoncée et vérifie le hash BLAKE3 du résultat. Les autres trames restent en JSON.

Le serveur Python préchauffe ses imports. Après avoir forké le fils qui exécute un module, il importe ce module dans son propre processus pendant que le fils s'exécute : le fils suivant du même module démarre donc avec lui déjà chargé.

## Options envisagées et écartées

Un client ssh en Rust (`russh`) a été mesuré contre un hôte : un nouveau canal sur une connexion `russh` ouverte a pris 45 ms, une nouvelle session sur un `ControlMaster` OpenSSH 53 ms. Les 8 ms gagnés auraient coûté 115 crates de plus dans l'arbre de dépendances de l'agent (de 63 à 178), un échec de `cargo deny` sur RUSTSEC-2023-0071, et la prise en charge de la configuration de l'opérateur : `Match` ignoré, `Include` qui fait échouer tout le fichier, `ProxyJump` analysé mais non utilisé, pas de `HostKeyAlias`, pas de `/etc/ssh/ssh_config` ni de `known_hosts` global, pas de GSSAPI dans un build statique. Garder OpenSSH laisse tout `~/.ssh/config` fonctionner comme pour `ansible-playbook`.

Un format de trame binaire n'aurait presque rien économisé. Encoder et décoder les trames d'une exécution complète des rôles a pris de 4.8 à 5.3 ms en JSON, de 3.9 à 4.6 ms en MessagePack avec noms de champs et de 5.9 à 6.6 ms en CBOR, sur une exécution de 12.5 s. MessagePack sans noms de champs n'a pas pu relire sa propre sortie. Protobuf n'a pas de moyen propre de porter le `Map<String, Value>` qui représente 80 % des octets. JSON reste ; seul le blob a changé.

QUIC exigerait un démon et un port en écoute sur chaque hôte. La promesse de Volant est un agent statique démarré par l'accès ssh que vous avez déjà.

io_uring trouverait peu à accélérer, puisque l'agent lui-même fait peu d'entrées-sorties. Son temps est dans les modules qu'il exécute et les processus qu'ils démarrent.

`gc.freeze()` et un fils forké à l'avance ont été mesurés dans le serveur Python. Chacun a économisé 1 ms ou moins par tâche, et aucun n'a été retenu.

## Conséquences

- Une tâche dans un sous-ensemble natif n'exécute plus Python. Un hôte où chaque tâche reste dans l'un d'eux ne démarre jamais le serveur Python, comme sur la charge des rôles.
- Le chemin Python reste la référence pour tout le reste, et le sous-ensemble de chaque module natif est documenté dans son code source. Un changement d'arguments peut faire passer une tâche d'un chemin à l'autre sans changer son résultat.
- `[volant] native_modules = false` envoie chaque tâche à Python, et `--facts python` force le `setup` Python, pour comparer les deux chemins sur une vraie exécution. `--profile` et `VOLANT_PROFILE_JSON` montrent quel chemin chaque tâche a pris et pourquoi elle a rendu la main.
- Le répertoire des sockets ssh (`$XDG_RUNTIME_DIR/volant-cm`, ou `/tmp/volant-cm-<uid>` en mode 0700) est un endroit de plus où le contrôleur écrit. Une connexion maître survit 30 secondes à l'exécution.
- Au démarrage, Volant relève sa limite de fichiers ouverts jusqu'à la limite stricte, puisque chaque lien tient trois descripteurs. Une limite stricte de 1024 plafonne encore une exécution à environ 330 liens.
- Un contrôleur et un agent de versions de protocole différentes se refusent mutuellement, comme avant. Volant envoie toujours son propre agent : cela ne compte donc que pour un agent construit à la main.

## Résultats

Mesuré le 2026-09-27 avec `just bench` et `just bench-k3s` : les quatre rôles en 0.529 s contre 26.435 s pour ansible-core 2.19.12 avec `pipelining = True` (un rapport de 49.97), et k3s-ansible en 23.477 s contre 50.519 s (2.15). La configuration, les lignes de commande et une comparaison étape par étape avec le 2026-09-24 se trouvent sur la [page de performances](/fr/docs/project/performance/).
