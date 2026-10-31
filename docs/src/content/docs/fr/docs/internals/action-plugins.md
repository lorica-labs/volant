---
sourceHash: 5a320c445d7e
title: Plugins d'action
description: Comment Volant exécute les modules qu'ansible-core exécute par un plugin d'action sur le contrôleur, en Rust ou sur le pont Python.
---

La plupart des modules livrés avec ansible-core s'exécutent tels quels : le contrôleur envoie une charge utile, l'hôte l'exécute, le résultat revient. Quelques-uns fonctionnent autrement : ansible-core décide lui-même de ce qu'il envoie, parfois après avoir d'abord posé une question à l'hôte, et le nom du module dans la tâche n'est que le point d'entrée. `package` en est un : ce qui s'exécute est `apt` ou `dnf`, choisi une fois que l'hôte a dit lequel il a. `copy` en est un autre : savoir si quoi que ce soit voyage vers l'hôte dépend d'une somme de contrôle que l'hôte rapporte d'abord.

Volant en exécute huit lui-même : `copy`, `dnf`, `fetch`, `package`, `reboot`, `service`, `template` et `unarchive`. Chacun est une petite machine à états sur le contrôleur : on lui demande l'étape suivante, on lui remet le résultat de la précédente, jusqu'à ce qu'il ait le résultat de la tâche elle-même. Chaque étape qu'il franchit est un module ordinaire de l'union de l'exécution, envoyé sur le lien que la tâche a déjà, une étape par aller-retour, sauf le `stat` et le `file` de `copy` décrits plus bas. Le résultat par lequel un plugin se termine suit le même chemin que tout autre résultat de module : rien de ce qu'un hôte a répondu n'atteint donc les variables d'un playbook par une voie particulière.

Le mot-clé `timeout:` d'une tâche borne l'ensemble de l'élément de cette façon, pour chacun des huit : chaque sous-tâche et chaque attente du retour de l'hôte, reconnexion de `reboot` comprise. Quand ce délai est épuisé, la tâche se termine là avec `Task failed: Timed out after <seconds> second(s).`, jamais avec ce que le plugin aurait fait du temps qui lui restait.

Les noms qu'ansible-core exécute aussi par un plugin d'action, et tous les plugins d'action des collections du corpus, passent par une seconde voie : le Python du plugin lui-même, issu d'ansible-core, dans l'assistant du contrôleur. Voir [Sur le pont](#sur-le-pont) plus bas. Voir [Modules](/fr/docs/reference/modules/) pour ce que cette version exécute nativement, sur le contrôleur et sur le chemin Python préchauffé.

## `copy`

Un fichier du contrôleur, ou un `content:` écrit directement dans la destination, envoyé à l'hôte seulement s'il diffère.

Le plugin exécute d'abord `stat` sur la destination, en demandant un SHA-1 sauf avec `force: false` : dans ce cas il n'en demande aucun et laisse intacte une destination existante, quel que soit son contenu. Il compare cette somme à celle des octets locaux. Une destination qui contient déjà les mêmes octets reçoit `file`, pour définir les attributs que la tâche a demandés ; tout le reste reçoit les octets déposés sur l'hôte et une sous-tâche `copy`. Seul ce dernier cas envoie quelque chose. Le `file` part dans le même aller-retour que le `stat` : l'agent ne l'exécute que si le `stat` a trouvé un fichier ordinaire avec cette somme, donc une destination qui contient déjà les octets coûte un seul aller-retour. Dans une boucle de `copy` ou de `template` où chaque élément nomme son propre `dest`, sans `until`, `break_when`, `pause`, `timeout`, `async` ni délégation, les éléments franchissent chaque étape ensemble, dans le même aller-retour : une boucle sur des fichiers que l'hôte a déjà coûte un seul aller-retour.

`remote_src: true` saute tout cela : `copy` s'exécute seul, avec les arguments de la tâche, et `src` nomme un fichier déjà présent sur l'hôte.

Un fichier de plus de 16 Mio n'est jamais lu en entier : le plugin le hache sur le contrôleur et l'envoie à l'hôte par morceaux de 16 Mio, que l'agent écrit sur le disque à mesure qu'ils arrivent. `unarchive` envoie son archive de la même façon.

Rejeté avant que l'hôte soit interrogé, avec les mots d'ansible-core :

- `src (or content) is required`
- `dest is required`
- `src and content are mutually exclusive`
- `can not use content with a dir as dest`
- `copying a directory is not supported yet: <src>`. Le plugin Rust ne copie pas de répertoire : la tâche s'exécute donc sur le pont, qui le fait.
- Un `src` qui ne nomme rien sur le contrôleur, enveloppé en `Unexpected AnsibleActionFail error: Could not find or access '<src>' ...`.

## `template`

Un modèle rendu sur le contrôleur, puis remis au même chemin que celui de `copy`.

Le plugin trouve le modèle sous `templates/`, le rend une fois avec les variables de la tâche et ses propres délimiteurs, et copie le résultat comme `copy` copie un `content:` : `stat`, puis `file` ou un `copy` avec dépôt. Le rendu a lieu exactement une fois ; rien ne rend le résultat une seconde fois, de sorte qu'une valeur fournie par un hôte géré arrive dans le fichier comme du texte et non comme un modèle à part entière.

Quatre variables sont définies pour le rendu, en plus de celles de la tâche : `ansible_managed` (sauf si la tâche l'a déjà définie), `template_path` (`src` tel que la tâche l'a écrit), `template_fullpath` (le chemin trouvé sur le contrôleur) et `template_destpath` (`dest` tel que rendu, voir plus bas). `template_host`, `template_uid`, `template_run_date` et `template_mtime`, qu'ansible-core définit aussi, ne le sont pas.

`trim_blocks` est activé par défaut et `lstrip_blocks` désactivé, comme dans ansible-core, et le saut de ligne final du fichier est conservé. `newline_sequence` accepte `\n`, `\r` ou `\r\n`, l'écriture échappée de quatre caractères étant lue comme la littérale. Les six options de délimiteurs (`variable_start_string` et ses cinq semblables) et une première ligne `#jinja2:` dans le fichier modèle sont lues comme ansible-core les lit, voir [Rendu d'un fichier modèle](/fr/docs/variables/templating/#rendu-dun-fichier-modèle). `output_encoding` écrit en UTF-8, UTF-8 avec signature, Latin-1, ASCII, cp1252, ISO 8859-15, UTF-16 et UTF-32, et signale tout autre encodage comme pas encore disponible. `{% include %}` et `{% import %}` cherchent dans `templates/` puis dans le répertoire lui-même, pour chaque entrée de `ansible_search_path`, le répertoire du playbook et celui du fichier modèle, comme ansible-core.

Rejeté avant que l'hôte soit interrogé :

- `'state' cannot be specified on a template`
- `src and dest are required`
- `newline_sequence needs to be one of: \n, \r or \r\n`
- `Block, variable and comment start strings must be different.`
- `TemplateOverrides.<name> must be <class 'str'> instead of <class 'NoneType'>`, pour une option de délimiteur vide ou nulle
- `output_encoding '<encoding>' is not supported yet on 'template': only utf-8, ... are`
- Un `src` qui ne nomme rien sur le contrôleur, avec les mots d'ansible-core, sans l'enveloppe `AnsibleActionFail` cette fois.

## `package`

Le gestionnaire de paquets de l'hôte nomme le module qui s'exécute réellement.

Le nom vient de `use:`, sauf s'il vaut `auto` ; puis de la variable `ansible_package_use` de l'hôte ; puis de `ansible_facts.pkg_mgr` de l'hôte sur lequel le module s'exécute ; puis d'un `setup` filtré sur ce seul fact, relancé pour chaque tâche et jamais conservé. Le module que le nom désigne est celui qui s'exécute, avec `use` retiré de ses arguments ; un `setup` qui échoue termine la tâche avec `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`, et un nom qui ne mène à rien que cette version puisse envoyer échoue avec `Could not find a matching action for the "<name>" package manager.` Un nom rapporté par un hôte n'est jamais qu'une clé de recherche dans cette liste fermée : `setup`, `apt`, `dnf`, `dnf5`. L'union contient les quatre backends plutôt que le seul dont une exécution aura besoin, car rien n'est construit une fois les facts connus : un plugin ne peut jamais choisir que parmi ce qui a déjà voyagé.

## `dnf`

Encore le gestionnaire de paquets de l'hôte, cette fois pour choisir entre deux noms de modules au lieu de quatre.

Le nom du backend vient de `use:`, ou de `use_backend:` quand `use` est absent ; les deux ensemble sont rejetés : `parameters are mutually exclusive: ('use', 'use_backend')`. `auto` et `yum` lisent `ansible_facts.pkg_mgr` de l'hôte sur lequel le module s'exécute, collecté par un `setup` filtré sur ce seul fact quand il n'est pas déjà connu ; tout autre nom est pris tel quel. `yum`, `yum4` et `dnf4` exécutent tous le module `dnf`, `dnf5` exécute `dnf5`, et un nom qui n'est toujours aucun de ceux-là échoue avec le message d'ansible-core en deux lignes, accolade fermante comprise :

```text
Could not detect which major revision of dnf is in use, which is required to determine module backend.
You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})
```

Un `setup` qui échoue termine la tâche avec sa propre cause, `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`. Quand la tâche n'est pas déléguée, son résultat contient `ansible_facts.pkg_mgr` défini à ce que le `setup` a répondu, comme le résultat d'ansible-core ; le résultat d'une tâche déléguée n'en contient aucun, car la réponse d'un hôte délégué atterrirait sinon dans les facts d'un mauvais hôte.

Le module `yum` n'est pas un module à part entière dans ansible-core 2.19 : `yum`, `ansible.builtin.yum` et `ansible.legacy.yum` sont des redirections vers l'action `dnf`, et une tâche qui en nomme un exécute ce plugin. Une clé `module_defaults` pour `yum` s'applique à une tâche `dnf`, et l'inverse.

## `fetch`

Un fichier lu depuis un hôte, écrit sur le contrôleur sous `dest` et nulle part ailleurs.

Sans `become`, le plugin exécute d'abord `stat` sur la source et laisse tranquille un fichier local déjà identique ; sous `become`, il passe directement à `slurp`. Les octets reviennent par `slurp`, décodés strictement, jamais rendus et jamais placés dans une variable. Un fichier de plus de 2 Mio, près du maximum que `slurp` transporte, est relu à la place par morceaux d'au plus 16 Mio, directement dans un fichier à côté de la destination. Sous `become`, cela arrive une fois que `slurp` a dit que le fichier est trop gros, après un `stat` pour sa somme. La destination est `dest/<inventory_hostname>/<src>`, sauf si `flat: true` nomme directement `dest` lui-même ou, avec une barre oblique finale, `dest/<basename of src>`.

Mesuré sur ansible-core 2.19.12, un `src` relatif qui remonte avec `..` fait écrire ansible-core en dehors de `dest` sur le contrôleur : un `src` de `../../../../../../../../tmp/x` sans `flat` écrit `/tmp/x`. Ici, chaque chemin candidat est construit et normalisé sans toucher au système de fichiers, et refusé s'il n'est pas sous `dest`, avant même qu'un répertoire soit créé :

- `the fetched path <p> is outside '<dest>'`

Ceci est vérifié deux fois : sur le `src` que le playbook a écrit, puis sur le chemin que l'hôte renvoie, qui n'a pas à correspondre. Une fois la protection active, un `dest` dont le rendu a lu un hôte géré est refusé avant que quoi que ce soit soit demandé, sauf si `[volant] host_named_dest_dirs` le couvre (voir [Un dest nommé par un hôte géré](#un-dest-nommé-par-un-hôte-géré)), et il en va de même pour un `inventory_hostname` de boucle quand un hôte l'a défini. Un `inventory_hostname` qui n'est pas un seul composant de chemin est toujours refusé :

- `the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it`
- `the inventory_hostname '<name>' is not one path component, and fetch files what it fetches under it`

Un lien symbolique n'importe où sous `dest` est refusé plutôt que suivi, sur le chemin construit à partir du playbook comme sur celui que l'hôte renvoie.

`fail_on_missing`, activé par défaut comme dans ansible-core, fait échouer la tâche sur une source absente ou un répertoire ; `fail_on_missing: false` transforme l'un ou l'autre en résultat `ok` portant à la place le message d'ansible-core. Dans les deux cas, toute autre erreur, comme une lecture que l'hôte refuse, fait toujours échouer la tâche. `validate_checksum` compare le SHA-1 du fichier écrit à celui de l'hôte avant de le conserver ; une différence fait échouer la tâche et supprime ce qui a été écrit, contrairement à ansible-core, qui laisse le fichier erroné là où il l'a écrit. Un fichier remplacé garde son mode actuel : un fichier `0600` récupéré de nouveau reste `0600`.

### Un dest nommé par un hôte géré

Un `dest` est nommé par un hôte géré quand son rendu a lu quelque chose qu'un hôte a fourni. Un cas réel est `dest: "{{ tmp.path }}/out/"`, où `tmp` est le résultat enregistré d'une tâche `tempfile`. ansible-core y écrit, et `fetch` aussi par défaut. Un hôte qui répond à `tempfile` par `/home/<operator>/.ssh` fait écrire le contrôleur en tant que l'opérateur sous ce répertoire. Il en va de même pour une variable de boucle nommée `inventory_hostname` sur une liste enregistrée, qui nomme le répertoire sous `dest`.

Deux paramètres activent la protection, pour une exécution où un hôte est moins digne de confiance que la personne qui lance le playbook. Le premier est `[volant] host_named_dest_dirs` (environnement : `VOLANT_HOST_NAMED_DEST_DIRS`), une liste de répertoires absolus séparés par des deux-points, non définie par défaut. Le second est `[volant] host_named_paths` défini à `contained` ou `refuse`, qui joue le rôle d'une liste vide quand `host_named_dest_dirs` n'est pas défini : tout `dest` de ce genre est alors refusé. Une liste définie s'applique toujours, quoi que dise `host_named_paths`, et une valeur vide refuse tout `dest` de ce genre. Avec la protection active, un `dest` nommé par un hôte n'est écrit que si le chemin auquel il aboutit dans le système de fichiers se trouve strictement à l'intérieur de l'un des répertoires :

- Le `dest` doit être absolu et ne contenir aucun `..`. Un chemin relatif et un chemin qui commence par `~` sont refusés, jamais développés.
- La partie la plus profonde du chemin qui existe est résolue à travers tous les liens, et le reste est ajouté. Un lien qui sort du répertoire, un lien qui ne pointe nulle part, le répertoire lui-même et un voisin dont le nom a le sien pour préfixe (`/srv/fetch-evil` pour `/srv/fetch`) sont refusés. La vérification est refaite juste avant l'écriture.
- L'écriture descend depuis le répertoire de la liste et refuse tout lien en dessous, même s'il reste à l'intérieur, ainsi que tout répertoire existant en dessous qui appartient à un autre utilisateur ou que tous les utilisateurs peuvent écrire sans sticky bit. Un autre utilisateur pourrait sinon remplacer l'un ou l'autre par un lien entre la vérification et l'écriture. Les répertoires que `fetch` crée en chemin sont les siens.
- Un répertoire de la liste que le groupe ou les autres utilisateurs peuvent écrire ne contient rien, sauf si son sticky bit est défini, comme sur `/tmp`. Cela est vérifié quand une tâche utilise le répertoire.
- Une entrée relative, vide, écrite avec `~` ou `..`, ou `/`, arrête l'exécution au démarrage avec le code de sortie 5.
- Un `dest` que le playbook a écrit n'est pas soumis à la liste, et le paramètre ne change rien pour `src`, qui nomme un fichier sur l'hôte.

```
the 'dest' of this task was named by a managed host, and '/home/op/.ssh/x/' is not inside any of the directories (host_named_dest_dirs = /srv/fetch)
```

Nommez un répertoire que seul l'opérateur peut écrire. Avec `/tmp` dans la liste, un hôte peut choisir n'importe quel nom en dessous.

Seul `fetch` est soumis à la liste. `copy` et `template` avec `delegate_to: localhost`, ainsi que `synchronize` en mode pull, écrivent aussi un `dest` sur le contrôleur. Ils ne sont pas refusés quand un hôte l'a nommé, comme dans ansible-core et comme auparavant.

## `service`

Le système d'init de l'hôte nomme le module, de la même façon que le gestionnaire de `package` : `use:` en minuscules sauf `auto`, puis `ansible_facts.service_mgr`, puis un `setup` filtré sur `ansible_service_mgr`. Contrairement à `package`, un nom que ni `systemd`, `systemd_service` ni `sysvinit` ne porte retombe sur `service` au lieu de faire échouer la tâche, et il en va de même pour un `setup` qui a échoué. Nommer `systemd` lui-même retire les arguments que ce module n'accepte pas : `pattern`, `runlevel`, `sleep`, `arguments`, `args`, chacun avec son propre avertissement, `Ignoring "<name>" as it is not used in "systemd"` ; une tâche qui écrit `ansible.builtin.systemd` les garde.

Une exécution qui nomme à la fois `package` et `service` place chaque backend des deux dans l'union avant qu'un seul fact soit connu. Mesuré sur le constructeur de charges utiles d'ansible-core 2.19.12, cela représente environ 170 KB de plus envoyés une fois par lien que ne coûteraient `apt` et `systemd_service` seuls.

## `unarchive`

Une archive lue sur le contrôleur, ou déjà sur l'hôte, extraite par le module `unarchive`.

`copy:` se replie dans `remote_src` (l'inversion propre à ansible-core), et les deux ensemble sont rejetés : `parameters are mutually exclusive: ('copy', 'remote_src')`. Un `creates:` qui existe déjà sur l'hôte fait ignorer la tâche, vérifié par un `stat` avant même d'examiner `dest` : `{"changed": false, "msg": "skipped, since <path> exists", "skipped": true}` ; un `stat` qui échoue est lu comme « absent », comme un test d'existence du shell lirait une commande qu'il n'a pas pu lancer, et non comme un échec de la tâche. La destination doit exister et être un répertoire, sinon la tâche échoue avec `dest '<dest>' must be an existing dir`. Ce n'est qu'ensuite que la source est cherchée, rejetée si un hôte l'a nommée, déposée et envoyée, sauf si `remote_src` nomme un fichier déjà présent.

Rejeté avant que l'hôte soit interrogé :

- `src (or content) and dest are required`
- `a '~' path is not supported yet on 'unarchive'`, pour `dest` ou `creates`. La tâche s'exécute alors sur le pont.
- Un `src` qui ne nomme rien sur le contrôleur : `Task failed: Could not find or access '<src>' ...\nIf you are using a module and expect the file to exist on the remote, see the remote_src option`
- `src is a directory, not an archive: <src>`, pour un `src` trouvé comme répertoire sur le contrôleur.

## `reboot`

Redémarre l'hôte, puis attend qu'il réponde avec un nouvel identifiant de démarrage et que sa commande de test réussisse, avant la fin de la tâche.

Relevé dans `plugins/action/reboot.py` d'ansible-core 2.19.12 et mesuré par rapport à lui, sous `become`, sur un hôte revenu de lui-même : un `setup` filtré sur la distribution, `cat /proc/sys/kernel/random/boot_id` exécuté comme commande simple, un `find` sur cinq chemins de recherche pour le programme d'arrêt, la commande d'arrêt elle-même, puis l'identifiant de démarrage relu jusqu'à ce qu'il change, et enfin la commande de test. Tout cela passe par la connexion existante de l'hôte, par `raw`, comme la commande de bas niveau d'ansible-core passe par la sienne.

Exécuté contre `local`, la connexion que le contrôleur utilise pour lui-même, la tâche est rejetée avant toute autre chose : `Running reboot with local connection would reboot the control node.`

Une fois la commande d'arrêt partie, cette version abandonne la connexion et en ouvre une nouvelle pour surveiller l'hôte, au lieu de garder l'ancienne ouverte pendant le redémarrage. `post_reboot_delay` est le temps d'attente avant la première vérification, et `reboot_timeout` (600 secondes par défaut) borne l'attente entière, à moins que le `timeout:` de la tâche la coupe plus tôt. `connect_timeout`, quand la tâche en donne un, ne fixe que le `ConnectTimeout` SSH de chaque reconnexion ; il ne borne plus la tentative elle-même. Un identifiant de démarrage vide n'est pas lu comme un nouveau démarrage : un hôte peut ne rien répondre juste avant de s'arrêter. Une fois l'identifiant de démarrage changé, `test_command` (`whoami` par défaut) doit encore réussir avant la fin de la tâche ; l'une ou l'autre attente qui s'épuise fait échouer la tâche avec le message d'ansible-core, `Timed out waiting for <what> (timeout=<reboot_timeout>)`. Un lien qu'un autre hôte de la même exécution a gardé ouvert vers l'hôte qui vient de redémarrer reçoit une vérification de vie supplémentaire avant sa prochaine utilisation, et est remplacé si le redémarrage l'a tué : ce pilote n'abandonne que ses propres liens, pas ceux d'un hôte délégué.

La ligne d'arrêt suit la table d'ansible-core : `shutdown -r <minutes> "<msg>"` sur la plupart des distributions, `reboot` seul sur Alpine, `shutdown -r +<minutes> "<msg>"` sur Void, ou `reboot_command` coupé à sa première espace quand la tâche en donne un. `search_paths` (cinq répertoires par défaut) est l'endroit où le programme d'arrêt est cherché quand `reboot_command` ne nomme pas déjà un chemin absolu ; ne pas le trouver fait échouer la tâche, en nommant tous les chemins parcourus.

Rejeté avant que l'hôte soit interrogé : un argument en dehors des neuf d'ansible-core (`boot_time_command`, `connect_timeout`, `msg`, `post_reboot_delay`, `pre_reboot_delay`, `reboot_command`, `reboot_timeout`, `search_paths`, `test_command`) fait échouer la tâche, trié et nommé : `Invalid options for reboot: <name>[,<name>...]`.

## Sur le pont

`add_host`, `assemble`, `gather_facts`, `group_by`, `script`, `set_stats`, `uri` et `wait_for_connection` exécutent le plugin d'action d'ansible-core dans l'assistant, tout comme chaque plugin d'action qu'une collection fournit (hors les plugins qui exigent un hôte Windows ou un autre type de connexion ; la page de [parité](/fr/docs/reference/parity/) les compte). `copy` et `unarchive` empruntent la même voie pour les deux cas que les plugins Rust refusent : `copy` d'un répertoire, et `unarchive` avec un `~` dans `dest` ou `creates`. Sous `host_named_paths = refuse`, un `src` qu'un hôte géré a nommé ne la prend jamais : le refus du plugin Rust s'applique. `async_status` n'est pas un plugin relayé par le pont : l'agent l'exécute, et un plugin relayé interroge ses jobs par son intermédiaire.

Le plugin demande ce dont il a besoin et le contrôleur répond : exécuter ce module, exécuter cette commande, lire une variable, créer un répertoire temporaire, déposer ce fichier. Chaque réponse est bornée par le `timeout:` de la tâche. Un plugin qui ne revient jamais, ou un assistant qui meurt, fait échouer cette tâche et non l'exécution ; un répertoire temporaire que le plugin a laissé sur l'hôte est supprimé. `add_host`, `group_by` et `set_stats` n'ont besoin d'aucun hôte et s'exécutent sans connexion. Ce qu'ils renvoient est appliqué par l'exécuteur : nouveaux hôtes et groupes pour les tâches suivantes, et statistiques personnalisées affichées après le récapitulatif. Une valeur d'`add_host` rendue à partir d'un fact reste une donnée : elle est conservée comme fact du nouvel hôte, jamais comme variable d'inventaire. Il en va de même d'un nom d'hôte ou de groupe d'`add_host` ou `group_by` construit à partir d'un texte qu'un hôte géré a écrit. Il en va de même de tout nom d'hôte ou de groupe qui contient `{`, que l'aient donné les fichiers d'inventaire, une source d'inventaire dynamique, `meta: refresh_inventory`, `add_host` ou `group_by` : `groups`, `hostvars`, `group_names`, les listes d'hôtes d'un play et `inventory_hostname` l'affichent comme du texte, comme le fait ansible-core. Dès qu'un tel nom existe, chaque lecture des listes qui le portent est une donnée. Lire un hôte par `hostvars[name]` n'est une donnée que pour cet hôte : un modèle imbriqué d'un autre hôte se rend donc encore.

L'assistant tient lieu de connexion du contrôleur, si bien que certaines choses qu'ansible-core permet à un plugin sont refusées, par leur nom, avant de faire quoi que ce soit :

- Un membre du contexte de connexion ou de play qui n'est pas l'un des rares que la couche d'adaptation porte : `Connection.<name> is not supported yet`, `the connection option <name> is not supported yet`, `the play context field <name> is not supported yet`, `the shell plugin <name> is not supported yet`.
- Un mode d'`async_status` autre que `status` et `cleanup`.
- Un module qui n'est pas du texte UTF-8 là où la couche d'adaptation le lit comme du texte.
- Les mots-clés de tâche `action`, `local_action` et `module_defaults` dans l'assistant : le contrôleur a déjà fusionné les valeurs par défaut.

Deux aspects de la connexion de l'assistant suivent ansible-core :

- `wrap_async` démarre un vrai job sur l'agent. Les jobs s'exécutent côte à côte, et un job qui dépasse sa limite `async:` est tué et se termine par `Timeout exceeded`.
- Une commande qu'un plugin envoie avec `sudoable=False`, le répertoire temporaire du plugin et les fichiers qu'il dépose s'exécutent sous l'utilisateur de connexion, sur un second lien qui n'élève pas les privilèges. Sous `become`, seuls les commandes et les modules `sudoable` s'exécutent avec élévation.

### Un plugin qui ouvre sa propre connexion

`ansible.posix.synchronize` sur ssh n'exécute pas son module sur l'hôte. Il ouvre une connexion `local` à lui et exécute le module sur le contrôleur, où `rsync -e ssh` copie les fichiers vers ou depuis l'hôte. L'assistant donne à un tel plugin les propres méthodes d'ansible-core pour exécuter un module, et garde sa connexion de substitution pour la connexion d'où vient la tâche.

Le plugin lit la clé et l'utilisateur du lien (la variable d'hôte, `--private-key`, `-u` ou `ansible.cfg`), si la tâche élève les privilèges et sous quelle identité (`sudo` devient `--rsync-path='sudo -u <user> rsync'`), `ansible_port`, `delegate_to` et `ansible_delegated_vars` et, avec `use_ssh_args`, les `ansible_ssh_common_args` et `ansible_ssh_extra_args` de l'hôte. `ssh_args` vaut la valeur par défaut d'ansible-core, `-C -o ControlMaster=auto -o ControlPersist=60s` : un `ssh_args` défini dans `ansible.cfg` n'est pas lu. `rsync` doit être présent sur le contrôleur et sur l'hôte. Push, pull, `delete`, `rsync_opts`, `delegate_to`, `become` avec `sudo`, `--check` et `--diff` donnent la sortie que donne `ansible-playbook`.

### Limites connues

- `wait_for_connection` sur un hôte hors service dès la première connexion se termine par `UNREACHABLE` pour un élément de plugin qui n'a jamais obtenu de lien ; ansible-core rapporte `failed` avec `timed out waiting for ping module test`.
- Un résultat relayé par le pont qui est `unreachable` au niveau du résultat s'affiche comme `ok`.
- Un résultat relayé ignoré porte `"failed": false`.
- Les avertissements qu'un plugin écrit avec `display` vont à la sortie d'erreur de l'assistant et non à la sortie de la tâche ; il en va de même pour l'avertissement `Error loading plugin` d'ansible-core pour un nom de collection inconnu.
- Un module que `runtime.yml` oriente vers un plugin d'action de nom différent s'exécute sous le nom de la tâche.
- Un `add_host` ou `group_by` délégué s'applique à l'hôte de la tâche.

## Un fichier source nommé par un hôte géré

Un `src` est nommé par un hôte géré quand son rendu a lu quelque chose qu'un hôte a fourni : un fact collecté, comme dans `src: "{{ ansible_os_family }}.j2"`, un résultat enregistré, ou un élément de `with_first_found` construit à partir de l'un d'eux. `[volant] host_named_paths` dit ce que `copy`, `template`, `script`, `unarchive` et `assemble` en font. Il s'applique au fichier lu sur le contrôleur, y compris sous `--check` et `--diff`. Pour `template`, une option de délimiteur (`variable_start_string` et les cinq autres) dont la valeur vient d'un hôte est utilisée sous `allow`, comme ansible-core l'utilise, et refusée sous `contained` et `refuse`. Une première ligne `#jinja2:` dans le modèle d'un auteur est lue dans tous les modes.

| Valeur | Ce que fait la tâche |
|---|---|
| `allow` (par défaut) | Cherche le fichier et l'envoie, comme ansible-core. |
| `contained` | Ne lit le fichier que s'il se trouve dans un répertoire `files` ou `templates` que la tâche parcourt. |
| `refuse` | Fait échouer la tâche avant que le moindre chemin soit cherché. |

`allow` signifie qu'un hôte géré peut choisir quel fichier lisible du contrôleur une tâche `copy`, `template` ou `script` lui envoie, exactement comme dans ansible-core 2.19.12. Mesuré là-bas, un `src` qui est une valeur enregistrée, le `stdout` d'une commande ou un fact envoie le fichier qu'il nomme, `~/.ssh` compris. Les rôles qui choisissent un modèle d'après un fact en dépendent. `contained` et `refuse` existent pour resserrer cela, pour une exécution où un hôte est moins digne de confiance que la personne qui lance le playbook.

Sous `contained`, les répertoires qui comptent sont les répertoires `files` et `templates` du rôle, des rôles dont il est une dépendance, du répertoire du fichier de la tâche (`tasks/files` dans un rôle) et du répertoire du playbook. Le reste de ces arborescences ne compte pas : `group_vars`, `vars`, `defaults`, un inventaire, `ansible.cfg` et un fichier de mot de passe vault ne sont jamais accessibles par un nom qu'un hôte a choisi, pas plus qu'un fichier gardé à côté du playbook. Les règles, dans l'ordre :

- Un nom qui commence par `~`, ou `~user`, est refusé. Les plugins d'ansible-core le développent en un répertoire personnel, ce que le texte du nom ne montre pas.
- Un nom qui contient un composant `..` est refusé, même s'il revient à l'intérieur. Le système de fichiers suit un lien avant de replier `..` : le chemin replié n'est donc pas celui qui serait ouvert.
- Un nom absolu doit se trouver sous l'un de ces répertoires et ne doit pas contenir de `$`, qu'ansible-core développe dans un nom absolu. `with_first_found` fournit un nom absolu de ce genre.
- Un nom relatif est cherché comme le plugin le cherche. Le premier fichier qui existe est celui qui est lu, et il doit se trouver sous l'un de ces répertoires. Un fichier de ce nom à côté du playbook, en dehors de `files`, est refusé quand il existe. En conséquence, le refus apprend à un hôte qu'un tel fichier existe. Cela se limite à un fichier à `<playbook>/<name>`, `<role>/<name>`, `<role>/tasks/<name>` ou `<parent role>/<name>` ; la réponse pour tout autre nom est le « introuvable » habituel.

La vérification lit le texte du chemin. Un lien que vous avez placé dans ces répertoires est votre propre fichier et est suivi. Une source sur l'hôte, comme `remote_src: true` pour `copy` et `unarchive`, ou `assemble` quand `remote_src` n'est pas défini à false, n'est pas un fichier du contrôleur et n'est pas vérifiée. `template`, `script` et les plugins d'une collection ne lisent aucun `remote_src` : il ne les exempte donc jamais. Pour `script`, le fichier est le premier mot de la ligne, ou de `cmd`. Pour `uri`, `src` et le `filename` de chaque champ d'un `body` multipart comptent.

Une tâche est refusée avant que le chemin soit cherché quand le mode ou le texte du nom en décide, car une réponse « introuvable » apprendrait déjà à un hôte si un chemin existe sur le contrôleur. Sous `refuse`, le message est :

```text
the 'src' of this task was named by a managed host, and a controller file a host chose is
never sent
```

Sous `contained`, il nomme la valeur et le paramètre :

```text
the 'src' of this task was named by a managed host, and '../up.j2' is not inside the
files or templates directories of the playbook or the role (host_named_paths = contained)
```

L'argument est `_raw_params` ou `cmd` pour un `script`. Tout autre mot pour le paramètre arrête l'exécution au démarrage avec le code de sortie 5 et nomme la clé, les trois mots et la valeur trouvée.

Le paramètre couvre les tâches listées ci-dessus. Il ne couvre pas `include_vars`, `vars_files` ni un lookup comme `file` dont un hôte a nommé le chemin. Il ne couvre pas non plus un `copy` avec `remote_src` ni un `slurp` délégué à `localhost` qui lit un chemin du contrôleur qu'un hôte a nommé. Ils se comportent comme dans ansible-core, et de la même façon sur toutes les versions antérieures de Volant.

Le mode change le fichier qui est lu, jamais la façon dont le texte d'un modèle est considéré comme fiable. Quand le `src` d'un `template` a été nommé par un hôte, `template_path` et `template_fullpath` sont les valeurs de cet hôte dans le rendu, comme `template_destpath` l'est pour un `dest`.

Un `fetch` dont l'hôte a nommé le `dest` suit lui aussi ce paramètre, bien que son `dest` soit un endroit où le contrôleur écrit et non un fichier qu'il lit : `allow` l'écrit, et `contained` et `refuse` ne l'écrivent que dans `host_named_dest_dirs` (voir [Un dest nommé par un hôte géré](#un-dest-nommé-par-un-hôte-géré)). Le `dest` des autres plugins n'est jamais qu'un endroit où écrire sur l'hôte : il n'est donc pas refusé ; `template_destpath` le porte dans le rendu comme valeur propre à l'hôte.

## Ce qui reste sur l'hôte

Un fichier ne voyage vers un hôte que lorsqu'une sous-tâche en a réellement besoin, et jamais plus loin que cette tâche.

Le contrôleur le dépose sous forme de blob adressé par son contenu, nommé par le hash BLAKE3 de ses octets, dans un répertoire privé à la seule connexion qui l'a envoyé : `stage-<host>-<pid>/`, dans le cache propre à l'agent, en mode 0700 et rejeté sauf si l'agent en est le seul propriétaire. Le module lit le fichier à cet endroit, sous un nom choisi par l'agent, jamais sous le chemin du contrôleur. Une fois exécutée la sous-tâche qui en avait besoin, qu'elle ait réussi, échoué, ou que le module ait déjà consommé le fichier (comme le fait `copy` quand il déplace sa source déposée en place), l'agent supprime ce qu'il en reste. Rien n'attend la fin de la connexion : un modèle rendu contient souvent un secret, et un fichier que personne n'a redemandé n'est pas laissé là pour que quelqu'un le trouve plus tard.

Le répertoire de dépôt entier de la connexion disparaît lui aussi quand la connexion se termine, quoi qu'il y reste. Un nouvel agent qui démarre sur un hôte supprime aussi les répertoires de dépôt des agents morts du même hôte qu'il trouve au démarrage : une exécution interrompue ne les laisse donc pas indéfiniment. Deux liens vers un même hôte, comme deux noms d'inventaire pour une machine ou deux hôtes qui délèguent au même, ne partagent jamais un répertoire de dépôt, puisque chacun est nommé par son propre identifiant de processus : le fichier déposé par une connexion n'est jamais visible par une autre.
