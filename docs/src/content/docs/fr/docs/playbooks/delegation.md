---
sourceHash: e7525bbdddd7
title: Délégation, tâches meta et environment
description: Exécuter une tâche une seule fois pour un lot ou sur un autre hôte, ce que fait chaque action meta, et comment environment et no_log façonnent une tâche.
---

Cette page répond à ces questions : comment exécuter une tâche ailleurs que là où le play travaille, comment piloter l'exécution elle-même avec `meta`, et comment définir des variables d'environnement ou masquer la sortie d'une tâche.

## run_once

`run_once` exécute une tâche sur un seul hôte du lot et donne à tous les autres hôtes ce qu'elle a produit, y compris tout ce qu'elle a enregistré.

```yaml
- name: Run database migrations once
  command: /opt/app/migrate.sh
  run_once: true
```

Les hôtes du lot se retrouvent devant une telle tâche avant que l'un d'eux ne l'exécute. L'élection doit être la même pour tous, et les autres hôtes doivent attendre celui qui exécute.

`run_once` peut être un modèle. Chaque hôte du lot le rend à tour de rôle, et le premier hôte pour lequel il est vrai exécute la tâche. Un modèle qui répond différemment dans le lot met fin à l'exécution avec le code 4.

Sous `strategy: free` et `host_pinned`, `run_once` est ignoré avec un avertissement et la tâche s'exécute pour chaque hôte. Une tâche `run_once` qui y échoue ne fait échouer que son propre hôte. Voir [Autres stratégies](/fr/docs/playbooks/how-a-play-runs/#autres-stratégies).

## delegate_to

`delegate_to` exécute une tâche ailleurs que sur l'hôte pour lequel elle s'exécute.

```yaml
- name: Take the host out of the load balancer
  command: /usr/local/bin/lb-drain {{ inventory_hostname }}
  delegate_to: lb1
```

La tâche passe par la propre connexion de l'hôte délégué, avec ses réglages de connexion. Le résultat appartient toujours à l'hôte sur lequel le play travaille : le récapitulatif compte cet hôte, et la ligne de résultat s'écrit `[web1 -> lb1]`.

- Une tâche sautée par `when` n'a jamais atteint l'hôte délégué : sa ligne `skipping:` n'a donc pas de flèche.
- La connexion de l'hôte qui délègue n'est pas ouverte pour une tâche déléguée : une tâche déléguée depuis un hôte injoignable s'exécute donc quand même.
- L'hôte délégué est cherché dans tout l'inventaire, pas seulement parmi les hôtes du play. Un play sur deux hôtes peut déléguer à un troisième qui reste hors du récapitulatif.
- Un nom que l'inventaire n'a pas est traité comme une cible SSH de ce nom, et signalé injoignable si cela échoue. Un hôte délégué injoignable retire de l'exécution l'hôte qui délègue.

`delegate_facts: true` stocke ce que la tâche a produit sur l'hôte délégué au lieu des hôtes qui l'ont exécutée.

Un module côté contrôleur comme `set_fact` ou `debug` s'exécute toujours sur le contrôleur, quoi que dise `delegate_to`. L'hôte délégué ne décide alors que du nom affiché sur la ligne de résultat et, avec `delegate_facts`, de l'hôte dont les facts sont écrits.

`action:` et `local_action:` prennent une chaîne (`command echo hi`), un mapping avec `module:` qu'Ansible juge obsolète, ou un nom de module seul avec les arguments sous `args:`. Un mot comme `module=command` après `action:` ne nomme aucun module. La tâche échoue quand un hôte l'atteint, avec le code 2, et un `rescue` ou `ignore_errors` prend l'échec, comme dans Ansible.

`delegate_to` et `delegate_facts` peuvent s'écrire sur un bloc et sur une tâche, pas sur un play, ce qui est aussi le cas dans Ansible. Les deux mots-clés se combinent : une tâche `run_once` avec un `delegate_to` s'exécute une fois, sur l'hôte délégué.

## Tâches meta

`meta` demande quelque chose au moteur au lieu d'exécuter un module. Toutes les actions d'Ansible 2.19 s'exécutent ici, et chacune affiche une bannière `TASK [meta]`. Une action qu'Ansible n'a pas affiche la bannière, puis échoue avec `invalid meta action requested: <action>` et un bloc `Origin`, avec le code 1, même si son `when` est faux. Une tâche meta n'ajoute rien au récapitulatif : sa ligne `skipping:` n'est pas comptée non plus.

| Action | Ce qu'elle fait | Le `when` de qui compte |
|---|---|---|
| `flush_handlers` | Exécute les handlers dont l'hôte a été informé, là où se trouve la tâche. | Celui de chaque hôte. |
| `noop` | Rien. | Ignoré, avec un avertissement. |
| `end_host` | L'hôte quitte le play. Plus de tâches, plus de sections `always`, plus de handlers. | Celui de chaque hôte. |
| `end_role` | L'hôte saute le reste du rôle où il se trouve, y compris une section `always` du rôle. | Celui de chaque hôte. |
| `end_batch` | Le lot se termine pour tous les hôtes, et le lot suivant démarre. | Celui du premier hôte. |
| `end_play` | Le play se termine pour tous les hôtes, avec les lots qui restent. | Celui du premier hôte. |
| `clear_facts` | Oublie les facts que les modules ont collectés, sur chaque hôte du lot. Les facts de `set_fact` restent. | Celui du premier hôte. |
| `clear_host_errors` | Pardonne les échecs et les hôtes injoignables du lot. Voir plus bas. | Celui du premier hôte. |
| `refresh_inventory` | Relit toutes les sources d'inventaire. Voir plus bas. | Ignoré, avec un avertissement. |
| `reset_connection` | Ferme la connexion de l'hôte, et la connexion `ssh` partagée avec elle. La tâche suivante rouvre les deux. | Ignoré, avec un avertissement. |

« Le premier hôte » est le premier hôte du lot qui est encore dans le play, dans l'ordre de l'inventaire. Les actions qui lisent son `when` s'exécutent une fois et affichent une seule bannière. Les autres s'exécutent pour chaque hôte et affichent une bannière par hôte encore dans le play.

`end_role` hors d'un rôle arrête l'exécution avant qu'elle ne démarre, avec le code 4 et le message d'ansible-core.

### Hôtes qui quittent le play

Un hôte qui se termine avec `end_host`, `end_batch` ou `end_play` n'a pas échoué, et le récapitulatif le montre tel qu'il était. Dans un `rescue`, l'échec qu'il a rattrapé compte comme rattrapé et l'hôte reste hors du code de sortie. Un hôte qui avait déjà échoué garde son échec. Les plays suivants s'exécutent de nouveau sur l'hôte.

### Pardonner les erreurs

`clear_host_errors` retire les échecs du lot du code de sortie. Le récapitulatif garde ses comptes : un hôte peut donc afficher `failed=1` dans une exécution qui se termine avec 0.

- Un hôte qui a échoué ne revient pas dans le play où il a échoué. Il revient dans les plays suivants.
- Un hôte devenu injoignable revient. Il reprend le play là où il s'était arrêté, et exécute les étapes qu'il a manquées une fois tous les autres hôtes terminés. S'il est de nouveau injoignable, il compte de nouveau.
- Un hôte qui était injoignable dans un play précédent revient aussi, à partir de la première tâche du play qui a effacé. Il est de nouveau injoignable dans les plays suivants, et le code de sortie dit 4 dans les deux cas.

### Recharger l'inventaire

`refresh_inventory` relit toutes les sources d'inventaire, dans l'ordre de `-i`. `groups` et `hostvars` changent aussitôt. Un hôte que les sources ont gagné rejoint à partir du play suivant, et un hôte qu'elles ont perdu quitte le play là où il en est, sans échouer. Les hôtes et groupes créés par `add_host` et `group_by` restent. Si les sources ne peuvent plus être lues, l'inventaire est vide à partir de là, comme dans Ansible : tous les hôtes partent, et les plays suivants ne correspondent à rien.

Les facts et les valeurs de `set_fact` restent comme ils étaient. Voir [Inventaires](/fr/docs/hosts/inventories/).

## environment

`environment` définit des variables pour le processus dans lequel un module s'exécute.

```yaml
- hosts: web
  environment:
    http_proxy: http://proxy.internal:3128
  tasks:
    - name: Fetch the release
      command: curl -fsSLO https://example.com/app.tar.gz
      environment:
        CURL_CA_BUNDLE: /etc/ssl/certs/internal.pem
```

Les couches sont fusionnées clé par clé, chacune par-dessus la précédente : le play d'abord, puis chaque bloc englobant de l'extérieur vers l'intérieur, puis la tâche. Chaque couche est rendue avec les variables de l'élément de boucle courant.

Les valeurs deviennent ce que le `str()` de Python en fait : `42` devient `"42"`, `true` devient `"True"` et `null` devient `"None"`. Une variable non définie dans `environment` fait échouer la tâche.

`environment` peut aussi être une liste de mappings, fusionnés dans l'ordre. Une entrée ou une valeur qui vaut `omit` est ignorée.

## no_log

`no_log: true` remplace la sortie de la tâche par la ligne censurée qu'affiche `ansible-playbook`, sur chaque ligne qu'elle aurait montrée et à tous les niveaux de verbosité. Cela couvre les lignes `ok:` et `changed:`, `fatal:`, `UNREACHABLE!`, la ligne de chaque élément de boucle et la sortie de `debug`. Le libellé d'un élément de boucle devient `(censored due to no_log)`.

Le bloc `[ERROR]:` affiché au-dessus d'un échec garde son préfixe, comme `Task failed: Action failed:`, et affiche `<error censored due to no log>` à la place du message. Ici Volant diffère d'`ansible-playbook` 2.19.12, qui ne censure que la raison d'un hôte injoignable et affiche en entier le message d'une tâche en échec : un secret dans un message de `fail` atteint donc le terminal.

`no_log` peut être un modèle. Chaque hôte le rend quand la tâche s'exécute. Dans une boucle, un élément qui masque masque tous les éléments, là où ansible-core masque chaque élément selon sa propre valeur.

La bannière de la tâche et le nom de la tâche ne sont pas masqués. La variable enregistrée et le récapitulatif gardent le vrai résultat : une tâche suivante peut donc encore le lire.

## Différences avec Ansible

- `delegate_to` est rendu une fois par tâche, pas une fois par élément de boucle. Un lot est un message vers un agent sur une connexion, et un hôte délégué par élément répartirait une boucle sur plusieurs connexions.
- `local_action` est `delegate_to: localhost` écrit autrement, et l'emporte sur un `delegate_to` de la même tâche.
- Quand l'hôte élu pour exécuter une tâche `run_once` ne peut pas être joint, les hôtes en attente quittent le play et l'exécution s'arrête là. Ansible affiche une bannière de play vide de plus. Aucun des deux moteurs n'élit un autre hôte, et le code de sortie est le même, 4.
- `run_once` combiné à `delegate_facts` stocke les facts sur l'hôte délégué, ce qui garde à `delegate_facts` un seul sens. Le comportement d'Ansible ici n'a pas été mesuré.
- `become`, `become_user` et `become_method` écrits à partir de l'élément de boucle sont rendus pour chaque élément. Quand tous les éléments élèvent les privilèges de la même façon, la boucle s'exécute. Quand ils diffèrent, la tâche échoue avec `loop items that escalate differently from one another are not supported yet`, là où Ansible élève les privilèges élément par élément : les éléments d'une tâche passent par une seule liaison, pour la même raison que l'hôte délégué.
- Une valeur d'`environment` qui n'est pas un mapping est sautée avec un avertissement qui nomme la valeur. Ansible affiche à la place toute la pile des couches où elle se trouvait.
- Cet avertissement n'est pas censuré par `no_log`. Ansible ne censure pas non plus son avertissement : cela correspond donc à ansible-core, mais gardez-le en tête avant de mettre un secret dans un `environment` qui pourrait ne pas être un mapping.
