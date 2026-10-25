---
sourceHash: 7b06690a9fe4
title: "N'étendre Ansible que par ce qui peut y revenir"
description: "Fiche de décision 0011."
sidebar:
  label: "0011 · Extensions et éjection"
  order: 11
---

| Statut | Date |
|---|---|
| proposé | 2026-10-25 |

## Contexte

Volant exécute les playbooks Ansible comme le fait ansible-core 2.19 ([décision 0002](/fr/docs/decisions/0002-ansible-core-2-19-as-reference/)). Une fois cela acquis, il peut offrir ce qui manque à Ansible. Le risque est un projet qui finit par dépendre d'une telle fonction et ne peut plus revenir à ansible-core. Une équipe adopte Volant plus facilement quand elle sait qu'elle peut repartir à tout moment.

Deux choses rendent ce retour possible. Le YAML reste la source : rien ne le remplace. Et chaque extension a une règle qui la réécrit en Ansible simple avec le même comportement, appliquée par une commande.

## Décision

### Deux sortes d'extension

Le contenu qu'Ansible sait charger, comme les filtres, les lookups et les modules, va dans la collection `volant.compat`. Il fonctionne tel quel sous ansible-core et n'a donc jamais besoin d'être éjecté.

Les mots-clés et la syntaxe que seul Volant comprend s'écrivent `volant.<mot-clé>`, comme clé du play, du bloc ou de la tâche auxquels ils s'appliquent :

```yaml
- name: one account per user
  volant.loop: "{{ users }}"
  block:
    - ansible.builtin.user:
        name: "{{ item }}"
    - ansible.posix.authorized_key:
        user: "{{ item }}"
        key: "{{ lookup('file', 'keys/' ~ item ~ '.pub') }}"
```

Une clé plutôt qu'un mapping `volant:`, parce que chaque extension est alors une clé qu'on peut rechercher et qui se réécrit seule. ansible-core refuse une telle clé bruyamment, mesuré sur 2.19.12 : `[ERROR]: 'volant.loop' is not a valid attribute for a Block`, avec la ligne. Un playbook qui utilise une extension ne tourne donc jamais à moitié compris sous ansible-core.

### Le contrat de réécriture

Chaque extension vient avec une règle de réécriture vers Ansible simple. Le comportement de l'extension est défini comme celui de sa réécriture sous ansible-core 2.19.12 : Volant n'invente pas de sémantique, il exécute plus vite la forme réécrite et garde l'original lisible.

Chaque extension vient aussi avec un test aller-retour. Le test exécute la forme réécrite sous ansible-core 2.19.12 et l'extension sous Volant, et compare la sortie et le récapitulatif. Aucune extension n'est fusionnée sans les deux.

### `volant eject`

```sh
volant eject [PATH...] [--out DIR] [--json]
```

Elle lit les playbooks et les rôles sous `PATH` (le répertoire courant par défaut) et écrit chaque fichier qui utilise une extension avec toutes ses extensions réécrites. Avec `--out DIR`, l'arborescence est écrite là ; sans, les fichiers sont réécrits sur place. La réécriture peut ajouter des fichiers, comme le fichier de tâches inclus d'une boucle réécrite. Ils vont sous `_volant/` à côté du fichier d'où ils viennent, nommés d'après la tâche et une empreinte de son contenu, si bien qu'éjecter deux fois donne la même arborescence.

Une extension dont la réécriture n'est pas exacte dans un contexte donné, comme `run_once` sous la stratégie `free` ci-dessous, est refusée avec son `fichier:ligne` et la raison, et rien n'est écrit. Le rapport liste chaque fichier réécrit et chaque fichier ajouté. `--json` donne un objet par fichier : `path`, `action` (`rewritten` ou `added`) et les extensions réécrites. Les codes de sortie sont 0 quand tout a été réécrit, 1 quand une extension a été refusée, et 4 quand un fichier ne peut pas être lu.

### `volant check --portable`

```sh
volant check --portable [PATH...] [--json]
```

Elle liste chaque ligne qu'ansible-core refuserait, avec son `fichier:ligne` et l'extension utilisée. Elle sort avec 0 quand il n'y en a aucune et 1 sinon. `--json` donne un objet par ligne : `path`, `line`, `extension`. Un job de CI peut s'en servir de barrière, et la sortie de `volant eject` doit la passer.

### Les premières extensions

Elles sont choisies d'après les votes publics du suivi d'issues d'ansible-core.

1. **Une boucle sur un bloc** (ansible/ansible#13262), `volant.loop` avec `volant.loop_control` sur un bloc. La réécriture déplace les tâches du bloc dans un fichier inclus et boucle sur l'inclusion :

   ```yaml
   - name: one account per user
     ansible.builtin.include_tasks: _volant/one-account-per-user-3f2a.yml
     loop: "{{ users }}"
   ```

   Mesuré sur 2.19.12, avec deux hôtes, sous `linear` et sous `serial: 1` : les inclusions sont développées d'abord, puis les tâches de chaque élément s'exécutent à tour de rôle, chaque tâche sur tous les hôtes. Un `when` à l'intérieur s'applique par élément, et `run_once` s'exécute une fois par élément. Sous la stratégie `free`, ansible-core n'honore pas `run_once` dans le fichier inclus : il avertit et exécute la tâche sur chaque hôte. Un `run_once` dans un `volant.loop` sous `free` est donc refusé par `eject` et signalé par `check --portable`, puisque Volant suit la réécriture.

2. **Un bloc comme handler** (ansible/ansible#14270) : un bloc dans `handlers:` qu'une tâche peut notifier par le nom du bloc. Mesuré sur 2.19.12, notifier un bloc par son nom échoue avec `[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list`. La réécriture transforme le bloc en un handler qui inclut ses tâches :

   ```yaml
   handlers:
     - name: restart app
       ansible.builtin.include_tasks: _volant/restart-app-91c0.yml
   ```

   Mesuré sur 2.19.12 : le handler notifié inclut le fichier pour chaque hôte notifié et exécute ses tâches dans l'ordre, comme le ferait un bloc. Il ne demande aucune nouvelle clé, puisque Volant accepte le bloc là où ansible-core ne peut pas s'en servir : il s'écrit donc en Ansible simple et se réécrit quand même.

3. **`collection uninstall`** (ansible/ansible#67759), une commande de `volant galaxy` plutôt qu'une syntaxe. Elle ne change rien dans un playbook, n'a donc besoin d'aucune réécriture et ne bloque jamais une éjection.

Les autres demandes sont laissées à des fiches ultérieures : `until` et `async` sur un bloc (#46203, #28997) n'ont pas de réécriture exacte qui garde les tentatives et l'attente, et un `import_playbook` conditionnel (#34281), une verbosité par tâche (#24215), le ET et le OU dans les expressions de tags (#11396) et une liste d'adresses pour `ansible_host` (proposals#97) demandent chacun leur propre mesure d'abord.

### Les rejeux shadow et le verrou

Un [rejeu shadow](/fr/docs/start/switch-without-risk/) rejoue un enregistrement d'ansible-core, et ansible-core refuse une extension : une extension n'apparaît donc jamais dans un enregistrement. Un projet qui veut les deux garde ses playbooks portables tant qu'il enregistre et rejoue, et commence à utiliser des extensions une fois le changement fait. Il peut aussi enregistrer la sortie de `volant eject` et rejouer celle-ci. Le verrou suit la même règle : il fixe le moteur qui exécute un projet et ne dit rien des extensions que le projet utilise.

## Conséquences

- Un projet peut quitter Volant à tout moment : `volant eject` lui rend une arborescence qu'ansible-core exécute, et `check --portable` le prouve.
- Chaque extension coûte une règle de réécriture et un test aller-retour avant d'être fusionnée, et un contexte où la réécriture n'est pas exacte est refusé, pas approché.
- La mise en œuvre suit dans ces issues, par taille :
  - lire les clés `volant.` et refuser une clé inconnue (petite) ;
  - la passe de réécriture, partagée par `eject` et par les tests aller-retour (moyenne) ;
  - `volant eject` (moyenne) ;
  - `volant check --portable` (petite) ;
  - `volant.loop` et `volant.loop_control` sur un bloc (moyenne) ;
  - un bloc comme handler (petite) ;
  - `volant galaxy collection uninstall` (petite).

## Mesures, 2026-10-25

ansible-core 2.19.12, deux hôtes en connexion locale, `gather_facts: false`. La boucle réécrite inclut trois tâches, `create`, `once` (avec `run_once: true`) et `key` (avec `when: item != 'bob' or inventory_hostname == 'h1'`), sur `[alice, bob]`.

Sous `linear`, les inclusions viennent d'abord, puis les tâches de chaque élément à tour de rôle :

```text
TASK [per user] ****************************************************************
included: _volant/per-user.yml for h1, h2 => (item=alice)
included: _volant/per-user.yml for h1, h2 => (item=bob)
TASK [create] ******************************************************************
ok: [h1] => {"msg": "create alice on h1"}
ok: [h2] => {"msg": "create alice on h2"}
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key alice"}
ok: [h2] => {"msg": "key alice"}
TASK [create] ******************************************************************
...
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key bob"}
skipping: [h2]
```

Sous `free`, `run_once` n'est pas honoré :

```text
[WARNING]: Using run_once with the free strategy is not currently supported. This task will still be executed for every host in the inventory list.
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [once] ********************************************************************
ok: [h2] => {"msg": "once alice"}
```

Sous `serial: 1`, chaque lot exécute toute la boucle, hôte après hôte. Un bloc dans `handlers:` ne peut pas être notifié par son nom, et sa réécriture s'exécute :

```text
TASK [change] ******************************************************************
[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list

RUNNING HANDLER [restart app] **************************************************
included: _volant/restart-app.yml for h1, h2
RUNNING HANDLER [stop] *********************************************************
ok: [h1] => {"msg": "stop"}
ok: [h2] => {"msg": "stop"}
RUNNING HANDLER [start] ********************************************************
ok: [h1] => {"msg": "start"}
ok: [h2] => {"msg": "start"}
```
