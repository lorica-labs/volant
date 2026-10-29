---
sourceHash: d61771341944
title: Référence du langage de modèles
description: Les méthodes Python, les règles sur les chaînes, les expressions régulières et les limites du Jinja2 de Volant, avec chaque différence avec ansible-core.
---

Cette page liste ce qu'un modèle peut appeler et où Volant diffère d'ansible-core. [Templating](/fr/docs/variables/templating/) couvre les règles de tous les jours ainsi que les filtres, tests et lookups que fournit Volant.

## Date et texte de remplissage

`now(utc=False, fmt=None)` donne la date et l'heure du contrôleur, jamais d'un hôte géré. Elle renvoie un `datetime` sans fuseau horaire, en UTC quand `utc` est vrai. Quand `fmt` est défini, elle renvoie à la place le texte que donne `strftime(fmt)` : `{{ now(utc=true, fmt='%Y%m%d') }}` vaut donc `20261019`. Le `datetime` a les attributs `year`, `month`, `day`, `hour`, `minute`, `second`, `microsecond` et `tzinfo`, les méthodes `strftime`, `isoformat`, `timestamp`, `weekday` et `isoweekday`, et se compare à un autre `datetime`. Comme résultat ou par `to_json`, il s'affiche `2026-10-19T23:13:50.741151`. Dans un texte, par `string` ou après `~`, il s'affiche avec une espace à la place du `T`, comme le fait Python. `strftime` utilise toujours la locale C : les noms de jours et de mois sont en anglais et `%c` donne `Mon Oct 19 23:13:50 2026`. ansible-core utilise la locale du contrôleur.

`lipsum(n=5, html=True, min=20, max=100)` donne `n` paragraphes de texte de remplissage aléatoire, comme le fait Jinja. Chacun a au moins `min` mots et moins de `max`. Avec `html` vrai, chaque paragraphe est entouré de balises `<p>` et un saut de ligne les sépare, sinon c'est une ligne vide. Un appel produit au plus 10 millions de mots.

## Méthodes Python

Un modèle peut appeler les méthodes que Python donne à ses valeurs, comme dans ansible-core. Une méthode que Volant n'exécute pas échoue en citant son nom : rien n'est ignoré sans message.

Ces méthodes de `str` s'exécutent, avec les arguments et les mots-clés que Python accepte et les termes de Python pour un appel erroné (`must be str or None, not int`, `substring not found`) :

| Groupe | Méthodes |
|---|---|
| Couper et joindre | `split`, `rsplit`, `splitlines`, `partition`, `rpartition`, `join`, `expandtabs` |
| Rogner et remplir | `strip`, `lstrip`, `rstrip`, `removeprefix`, `removesuffix`, `center`, `ljust`, `rjust`, `zfill` |
| Chercher | `find`, `rfind`, `index`, `rindex`, `count`, `startswith`, `endswith`, `replace` |
| Casse | `upper`, `lower`, `title`, `capitalize`, `swapcase`, `casefold` |
| Tester | `isalpha`, `isalnum`, `isdigit`, `isdecimal`, `isnumeric`, `isspace`, `islower`, `isupper`, `istitle`, `isascii`, `isidentifier`, `isprintable` |
| Autres | `format`, `format_map`, `translate`, `maketrans`, `encode` |

Les positions comptent des caractères, pas des octets, et `start` et `end` acceptent des valeurs négatives. `split(maxsplit=1)` et les autres formes avec mot-clé fonctionnent. `startswith` et `endswith` prennent un texte ou un tuple de textes. Une liste est acceptée comme tuple, parce que le Jinja de Volant n'a pas de type tuple.

Ce qui diffère de CPython :

- `casefold` est la mise en minuscules plus `ß` en `ss`. Ce n'est pas le repliement de casse complet d'Unicode.
- `isdigit`, `isalpha` et les autres tests hors ASCII suivent les tables Unicode de Rust. `'½'.isdigit()` est vrai ici et faux dans Python.
- Un champ de format peut lire un élément (`{0[a]}`, `{a[0]}`) mais pas un attribut (`{0.real}`).
- Une largeur, un nombre ou une précision qui ferait construire à une méthode plus de 256 MiB de texte (`'a'.ljust(10000000000)`, `'{:.300000000f}'.format(x)`) arrête la tâche avec une erreur qui nomme la limite. Python construit le texte jusqu'à épuisement de la mémoire de la machine. Un nombre trop grand pour un indice donne l'erreur propre de Python, `Python int too large to convert to C ssize_t`.
- `encode` produit une valeur bytes, et Volant n'exécute que quelques méthodes de bytes. Voir [Bytes](#bytes).
- Une séquence d'échappement avec antislash comme `'\n'` dans une chaîne de modèle est lue par le moteur de modèles. ansible-core 2.19 garde les deux caractères. Passez la valeur dans une variable quand le texte doit contenir un vrai saut de ligne.

### Listes et mappings

Les méthodes de `list` et de `dict` s'exécutent aussi : `append`, `extend`, `insert`, `remove`, `pop`, `sort`, `reverse`, `index`, `count`, `copy` et `clear` sur une liste, et `get`, `keys`, `values`, `items`, `pop`, `popitem`, `setdefault`, `update`, `copy`, `clear` et `fromkeys` sur un mapping. `fromkeys` est aussi une méthode du nom `dict` : `dict.fromkeys(['a', 'b'], 0)`. `keys()`, `values()` et `items()` donnent des listes. `sort` prend `reverse`, et `key` seulement sous la forme `none`. Les façons habituelles d'écrire une modification sont `{% set _ = items.append(x) %}` et `{{ items.append(x) or '' }}` : la méthode modifie la liste et répond `none`.

Un modèle modifie une liste ou un mapping sur place, et les autres noms de la même valeur voient la modification :

```jinja
{% set seen = [] %}
{% for host in groups['web'] %}{% set _ = seen.append(host) %}{% endfor %}
{% set alias = seen %}{% set _ = alias.append('db') %}
{{ seen }}   {# every web host, then db #}
```

Cela suit Python et ansible-core 2.19, qui s'accordent sur ces cas (chaque cas est testé contre ansible-core) :

- Une valeur lue depuis une variable est une copie pour la chaîne en cours de rendu. Une variable de play `x` qu'un modèle modifie avec `x.append(1)` n'est modifiée que pour ce modèle. La tâche suivante, le champ suivant de la même tâche et le modèle suivant la lisent telle qu'elle était. Aucune modification d'une variable ne survit au rendu.
- Dans un même modèle, `b = a` nomme une seule liste, et une modification par `b` se voit dans `a`. `b = a | list` et `b = a.copy()` font une nouvelle liste.
- Une liste ou un mapping à l'intérieur d'un autre est atteint par `x[0]`, `x.key` et `for item in x` comme le même objet : `d.a.append(1)` modifie donc `d`. Une valeur rangée par `append`, `extend`, `insert`, `update` ou `setdefault` est une copie, tout comme les valeurs que renvoient `get`, `items`, `values` et `copy`.
- `d.setdefault('k', []).append(1)` modifie `d`. `v = d.setdefault('k', [])` suivi de `v.append(1)` modifie `d` ici, et pas dans ansible-core.

Ce qui diffère d'ansible-core :

- Une liste ou un mapping qu'un modèle passe à une macro, à `namespace()` ou à un autre appel est le même objet dans Volant. ansible-core passe une copie : une macro qui modifie son argument ne modifie donc la liste de l'appelant qu'ici.
- ansible-core 2.19 garde l'erreur d'un appel dont le résultat n'est jamais lu. `{% set _ = [1].remove(5) %}` n'y est pas une erreur. Ici, c'en est une.
- Une liste construite par `a + b` ou par une tranche `a[1:]` ne peut pas être modifiée : écrivez `(a + b) | list`. L'erreur le dit.
- Une liste qu'un appel vient de construire et qu'aucune variable ne détient, comme `(x | list).append(1)`, est modifiée en tant que copie : l'appel répond comme Python, et rien d'autre ne change.
- Une liste qui grandit dans un `{% for %}` sur elle-même est lue jusqu'à sa longueur au début de la boucle. Une liste que `remove` ou `pop` fait rétrécir dans une telle boucle échoue avec une valeur indéfinie, là où ansible-core continue.
- Une liste qu'un filtre renvoie est une copie, comme dans ansible-core : `(items | first).append(1)` ne modifie donc pas `items`. Une tranche (`a[1:]`) partage avec `a` les listes qu'elle contient.
- Une liste ou un mapping qui est l'élément ou la clé de sa propre méthode est lu comme une copie de lui-même, et un mapping utilisé comme clé est une erreur (`unhashable type: 'dict'`), comme dans Python.
- `sort()` d'une liste qui contient `nan` est une erreur ici. ansible-core la trie.
- `{% do %}` est accepté ici et refusé par ansible-core 2.19, qui n'active pas `jinja2.ext.do`. Utilisez `{% set _ = ... %}` dans un modèle qui doit fonctionner sur les deux.
- `items()` et `popitem()` donnent des listes là où Python donne des tuples : ils s'affichent donc `['a', 1]` et non `('a', 1)`.
- `sort(key=...)` avec une fonction n'est pas disponible.

Des méthodes comme `.get()`, `.keys()`, `.values()` et `.items()` fonctionnent aussi sur d'autres valeurs de type mapping, comme `hostvars` et la variable de boucle.

### Bytes

`'text'.encode(encoding='utf-8', errors='strict')` produit une valeur bytes :

- `| length` compte les octets. `b[0]`, `b[-1]`, `| first`, `| list` et `{% for %}` donnent des entiers.
- `b.decode(encoding, errors)` et `b.hex(sep, bytes_per_sep)` donnent du texte. `.encode('ascii', 'ignore').decode('ascii')` supprime les caractères qui ne sont pas ASCII.
- `b64encode` lit les octets exacts. `b64decode`, `regex_replace`, `regex_search`, `regex_findall` et `quote` les lisent comme du texte UTF-8.
- Un résultat de type bytes devient son texte, et `x {{ b }}` aussi. `'x' ~ b`, `b | string` et `'%s' % b` donnent `b'abc'`, comme dans Python.
- `==`, `is sequence`, `is string` et `type_debug` (`bytes`) répondent comme dans ansible-core. `to_json` et `to_nice_json` échouent avec `Object of type 'bytes' is not JSON serializable`.

`encode` connaît `utf-8`, `ascii`, `latin-1`, `utf-16` et `utf-32` (`-le` et `-be` aussi), sous les alias que Python lit (`UTF-8`, `utf8`, `iso-8859-1`). Les gestionnaires d'erreur sont `strict`, `ignore`, `replace`, `backslashreplace` et `xmlcharrefreplace`. `decode` connaît `utf-8`, `ascii` et `latin-1`, avec `strict`, `ignore`, `replace` et `backslashreplace`. Une erreur reprend les termes de Python : `'ascii' codec can't encode character '\xe9' in position 0: ordinal not in range(128)`.

### L'opérateur `%`

Sur un texte, `%` le formate comme le fait Python : `'%s-%d' % (name, 3)`, `'%(host)s:%(port)d' % item`, `'%5.2f%%' % pct`. Sont lus les drapeaux `-`, `+`, espace, `0` et `#`, une largeur et une précision (chacune pouvant être `*` pour la lire dans les arguments), les modificateurs de longueur `h`, `l` et `L`, et les conversions `s`, `r`, `a`, `c`, `d`, `i`, `u`, `o`, `x`, `X`, `e`, `E`, `f`, `F`, `g`, `G` et `%`. Un appel erroné échoue avec les termes de Python : `not enough arguments for format string`, `not all arguments converted during string formatting`, `%d format: a real number is required, not str`.

Les entiers jusqu'à 128 bits qu'un modèle écrit ou calcule sont lus exactement (`'%d' % 18446744073709551615`). Un entier de plus de 63 bits dans un fichier de variables arrive au modèle sous forme de texte, ce que décide le lecteur YAML, pas `%`. Une largeur ou une précision au-delà de 256 MiB de texte est une erreur de tâche qui nomme la limite, comme pour les méthodes de `str`, et une valeur au-delà de ce qu'un indice peut contenir donne le `width too big` de Python.

Sur deux nombres, `%` prend le signe du diviseur, comme dans Python : `7 % -3` vaut `-2`. Un diviseur nul est une erreur. Le filtre `format` est `%` avec les arguments écrits à la suite, et lit les mêmes formats.

Volant n'a pas de type tuple : le côté droit de `%` est donc lu comme un tuple quand il est écrit en toutes lettres dans le modèle, sous la forme `(a, b)`, `(a,)` ou `['a', 'b']`. Une liste qu'une variable détient ou qu'un appel renvoie est un seul argument, comme dans Python : `'%s' % items` affiche la liste. `'%s %s' % pair`, avec une liste de deux éléments dans `pair`, est une erreur dans ansible-core, et ici. Un tuple s'affiche comme une liste : `'%s' % ((1, 2),)` vaut `[1, 2]` ici et `(1, 2)` dans ansible-core.

Deux nombres écrits en toutes lettres dans le modèle (`7 % -3`) sont repliés à la compilation du modèle, et ce repliement ne suit pas la règle de signe de Python et ne lève pas d'erreur sur un diviseur nul. Un nombre qui vient d'une variable le fait.

## Filtres d'ensembles

`difference`, `intersect` et `union` construisent un `set` Python dans ansible-core, dont l'ordre lui est propre : les petits entiers sortent triés par hasard, et les chaînes sortent dans un ordre qui change avec `PYTHONHASHSEED` d'une exécution à l'autre. Volant trie le résultat quand chaque élément est un entier, ce qui correspond à tous les cas d'entiers mesurés, et garde sinon l'ordre dans lequel les éléments sont apparus la première fois. Pour des chaînes, il n'est pas garanti que cet ordre corresponde à une exécution donnée d'ansible-core.

## Chaînes qui contiennent des blocs

Volant rend toute chaîne qui contient `{{`, `{%` ou `{#`, dans une variable, un fact, un argument de tâche, une boucle ou une variable supplémentaire. Il les rend comme ansible-core :

- `trim_blocks` est activé et `lstrip_blocks` est désactivé. Le saut de ligne après une ligne `{% ... %}` est supprimé : un scalaire de bloc avec une boucle `{% for %}` donne donc une ligne par passage et aucune pour les balises.
- Un résultat fait d'un seul `{{ }}` et de rien d'autre garde son type, même quand des instructions ou des commentaires l'entourent. `{% set a = [1, 2] %}{{ a }}` est une liste. Un résultat fait de texte, ou de plusieurs expressions, est une chaîne.
- Un résultat sans aucune sortie est `none`. `{% if false %}x{% endif %}` et `{# note #}` sont `none`, pas une chaîne vide.
- Un saut de ligne à la fin de la chaîne ne fait pas partie du résultat quand le résultat n'est pas une chaîne : `"{{ [1, 2] }}\n"` est une liste. `{{-` et `-}}` suppriment les espaces autour de la balise : `{{- x -}}` suivi d'un saut de ligne est donc aussi une seule valeur, et le `-` n'est jamais un signe moins.
- `{#` ouvre un commentaire. Un commentaire qui s'ouvre tout à la fin de la chaîne et ne se ferme jamais est accepté et ne veut rien dire. Tout autre élément non terminé est une erreur de syntaxe. Pour écrire `{#` comme texte, utilisez `{{ '{#' }}` : un résultat qui contient `{#` n'est pas rendu de nouveau, pas plus qu'une variable ou un fact enregistré dont le `{#` n'est suivi d'aucun `#}`.
- Une chaîne comme `echo ${#items[@]}` échoue avec une erreur de syntaxe, comme dans ansible-core. Enveloppez-la dans `{% raw %}` ou marquez-la `!unsafe`. ansible-core refuse une tâche `command` ou `shell` qui en contient une pendant qu'il charge les tâches, avec le code de sortie 4. Volant la refuse quand la tâche s'exécute, avec le code de sortie 2 : les tâches qui la précèdent s'exécutent donc d'abord.

Un résultat enregistré et tout autre texte venu d'un hôte géré restent du texte : ils ne sont jamais rendus. Voir [Valeurs fiables et non fiables](#valeurs-fiables-et-non-fiables). Une valeur sous `!unsafe` est une donnée et n'est jamais passée au templating. Une valeur `!vault` est déchiffrée, voir [Vault](/fr/docs/variables/vault/).

## Expressions régulières

`regex_replace`, `regex_search`, `regex_findall` et les tests `match`, `search` et `regex` lisent des motifs `re` de Python. Volant réécrit le motif pour la crate Rust `regex` et parcourt les correspondances comme le fait Python 3.7 et ses successeurs. La gestion des groupes, les correspondances vides, les chaînes de remplacement (`\1`, `\g<name>`, un `$` littéral) et les arguments, y compris les drapeaux passés par position, suivent ansible-core.

Certaines syntaxes Python n'ont pas d'équivalent dans la crate Rust. Un motif qui les utilise échoue avec une erreur qui nomme la construction, là où ansible-core l'exécute :

- l'anticipation et la rétrospection : `(?=`, `(?!`, `(?<=`, `(?<!`
- une référence arrière dans le motif : `\1`, `(?P=name)`
- les quantificateurs possessifs (`a++`), les groupes atomiques (`(?>`) et les groupes conditionnels (`(?(1)a|b)`)
- `\N{name}`, les drapeaux `(?a)` et `(?L)`, et `(?x:` comme drapeau à portée limitée
- une répétition paresseuse ou une alternative vide avant une autre (`a*?`, `|a`) quand une correspondance vide demande d'en essayer une plus longue au même endroit
- `$` dans un motif sans `multiline`, sur un texte qui a plus d'une ligne et se termine par un saut de ligne

Deux différences demeurent. Une correspondance à plusieurs groupes est une liste de listes ici et une liste de tuples dans ansible-core, ce qui ne se voit que quand la valeur est affichée comme texte. Le moteur de modèles de Volant lit `'\\'` dans un littéral de chaîne comme un seul antislash, là où ansible-core en garde deux.

## Accolades littérales

Le texte qu'écrit un bloc `raw`, ou une chaîne comme `'{{'`, est littéral : il n'est pas rendu de nouveau comme argument, dans une variable, dans une valeur de `set_fact` ou dans un élément de boucle. Une variable qui contient un bloc `raw` est lue jusqu'à sa fin d'abord : `a: "{{ b }}"` avec `b: "{% raw %}{{ x }}{% endraw %}"` donne donc `{{ x }}`.

Les accolades qu'une expression construit sont littérales aussi. Avec `x: a`, `{{ x ~ '{{ 1 + 1 }}' }}` donne `a{{ 1 + 1 }}`, `{{ x | replace('a', '{{ 7 * 6 }}') }}` donne `{{ 7 * 6 }}`, et `{{ '{{ ' ~ n ~ '_port }}' }}` donne `{{ web_port }}` quel que soit le contenu de `web_port`. Une variable qui contient un modèle est rendue quand elle est lue : une chaîne comme `a: "{{ b }}"` avec `b: "{{ c }}"` donne donc la valeur de `c`. Un résultat n'est rendu de nouveau que lorsqu'une lecture lui a donné un modèle qui n'était pas encore rendu : `hostvars['other']['name']` pour une variable qui en contient un, ou une variable laissée telle qu'écrite par la règle ci-dessus. Un texte qui joint une telle lecture à des accolades qu'il construit est alors rendu dans son ensemble.

## Filtres qui s'exécutent en Python

[Templating](/fr/docs/variables/templating/#filtres-tests-et-lookups-qui-nécessitent-python) dit quels noms ont besoin de Python. Voici les détails.

Un nom dans une ligne `import_playbook` passe aussi par Python, rendu avec les variables de `-e`.

Un plugin Python qui lève une exception échoue avec la chaîne d'ansible-core : la phrase du plugin dans un bloc, le message de l'exception dans le suivant. Un filtre ou un test qui lève une exception affiche la même chaîne dans un argument de tâche, dans un `when` et dans un fichier `template`.

Volant rend d'abord chaque argument et envoie le résultat comme donnée. Python ne le passe pas de nouveau au templating : une valeur venue d'un hôte géré qui contient encore `{{ ... }}` reste donc du texte brut. Les valeurs marquées `!unsafe` arrivent aussi comme données.

Un lookup qui s'exécute en Python reçoit aussi les variables du play. Volant lit la source du plugin et nomme les variables qu'il lit : les clés littérales qu'il cherche dans `variables`, et celles que nomment ses paramètres. Un plugin qui liste des variables, comme `community.general.merge_variables`, les reçoit toutes. Seul un tel plugin paie l'envoi de toutes les variables de l'hôte.

Chacun de ces cas fait échouer la tâche qui a fait l'appel, avec le message du processus Python dans l'erreur de la tâche :

- Le processus Python se termine ou est tué pendant un rendu.
- Une requête ou une réponse dépasse la limite de trame.
- Un plugin répond avec des octets qui ne sont pas de l'UTF-8.
- Le processus Python répond pour une autre requête.

L'appel suivant de l'exécution qui a besoin de Python échoue de la même façon au lieu d'attendre.

## Valeurs fiables et non fiables
Un modèle que vous avez écrit est du code. Une valeur qu'un hôte géré a renvoyée est une donnée. Volant garde les deux séparés comme le fait ansible-core 2.19 : une chaîne qu'un hôte contrôle ne s'exécute donc jamais comme modèle sur le contrôleur.

:::caution
C'est une frontière de sécurité. Si vous trouvez un moyen de faire évaluer des données d'hôte comme modèle sur le contrôleur, signalez-le en privé comme le décrit la [politique de sécurité](https://github.com/lorica-labs/volant/blob/main/SECURITY.md).
:::


### Quelles valeurs sont non fiables

| Non fiable, parce qu'un hôte ou l'exécution l'a produite | Fiable, parce que le playbook la porte |
|---|---|
| résultats de `register` et résultats de module | `vars` du play et de la tâche |
| facts collectés | `--extra-vars` |
| valeurs de `set_fact` | variables d'inventaire |
| `lookup('file')`, `lookup('pipe')`, `lookup('env')`, `lookup('template')` | `vars_files` et `include_vars` |

`!unsafe` marque une valeur du playbook comme donnée. Il est accepté sur une valeur et sur une clé de mapping, dans un play, dans `group_vars`, dans un fichier d'extra-vars et dans un inventaire YAML. Le filtre que le chargeur YAML utilise pour la marque est caché aux playbooks : l'appeler échoue avec `No filter named 'volant_unsafe'`.

### Ce qui change pour une valeur non fiable

Une valeur non fiable est rendue une seule fois. Si le résultat ressemble de nouveau à un modèle, il reste du texte et n'est pas rendu une seconde fois, ce que fait Ansible :

```yaml
# Say someone wrote {{ lookup('pipe', 'id') }} into /etc/motd on the host.
- command: cat /etc/motd
  register: motd

- debug:
    msg: "{{ motd.stdout }}"   # prints the braces as text, runs nothing
```

`debug: var:` évalue une expression. Quand le nom lui-même vient d'un hôte, la tâche échoue avec le message propre d'ansible-core :

```text
Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.
```

Écrire le nom vous-même, comme dans `debug: var: result.stdout`, fonctionne comme d'habitude. C'est `debug: var: "{{ from_a_host }}"` qui est arrêté.

Les conditions suivent la même règle. Un `when: "{{ ... }}"` qui se rend en une chaîne que vous avez écrite, comme `"1 == 1"`, est évalué une fois de plus comme condition. Si le rendu a lu quelque chose qu'un hôte a envoyé, la chaîne n'est jamais compilée, et la tâche échoue avec le même `Encountered untrusted template or expression.` Un `assert` dont le `that` est un modèle fonctionne de la même façon.

Un résultat de `lookup('template')` est aussi une donnée. Un modèle qui contient `{{ '{{ 1 + 1 }}' }}` donne le texte `{{ 1 + 1 }}`, pas `2`, avec les deux moteurs.

Un `delegate_to` qui se rend à partir d'une valeur d'hôte nomme un hôte avec ce texte, quel que soit son contenu. Le texte est l'`inventory_hostname` et l'`inventory_hostname_short` de l'hôte délégué, et rien ne le rend, même quand une variable de votre tâche place des accolades à côté. Il en va de même pour un nom qui contient des accolades quelle qu'en soit l'origine, et pour un hôte délégué qui n'est pas un hôte de l'inventaire, comme un hôte créé par `add_host`. Sous ssh, la tâche échoue avec `UNREACHABLE` et affiche le texte, comme dans ansible-core. Un hôte délégué qui se connecte en local s'exécute comme d'habitude.

Un chemin qu'un hôte géré nomme est un cas à part, parce que le contrôleur le lit ou l'écrit. Par défaut, le `src` de `copy`, `template`, `script`, `unarchive` et `assemble` et le `dest` de `fetch` se comportent comme dans ansible-core. Deux paramètres les durcissent : [`host_named_paths` pour un fichier source](/fr/docs/internals/action-plugins/#un-fichier-source-nommé-par-un-hôte-géré) et [`host_named_dest_dirs` pour un `dest`](/fr/docs/internals/action-plugins/#un-dest-nommé-par-un-hôte-géré).

### Granularité du suivi

Volant suit la confiance par variable, pas par chaîne. Ansible étiquette chaque objet chaîne : une chaîne fiable garde donc sa confiance à travers un modèle et ne la perd que lorsqu'une expression construit une nouvelle chaîne à partir d'elle. Dans Volant, une valeur est non fiable quand le rendu qui l'a produite a lu quelque chose qu'un hôte a apporté. La réponse est la même partout où un hôte a joué un rôle. Volant est plus permissif sur une valeur que le playbook a calculée à partir de son propre texte.

Un hôte géré ne peut pas non plus changer ses propres paramètres de connexion par les facts qu'un module renvoie. Volant retire les noms qu'ansible-core retire, comme `ansible_host`, `ansible_user` ou `ansible_ssh_common_args`, plus `ansible_remote_tmp`, et affiche le même avertissement.

## Limites sur YAML
Volant refuse un texte YAML qu'il ne peut pas charger sans risque. L'erreur nomme la limite. Un playbook, un fichier de variables, un inventaire et le texte que lit un filtre `from_yaml` suivent tous les mêmes limites.

- Un document imbrique des collections sur 489 niveaux au plus. ansible-core charge un fichier jusqu'à 489 niveaux et l'arrête à 490 avec `maximum recursion depth exceeded`, et Volant donne le même message. Un alias compte pour les niveaux du nœud qu'il nomme. Une collection de style flux s'arrête plus tôt, à 255 niveaux, parce que l'analyseur les compte dans un `u8`. ansible-core charge une collection de style flux jusqu'à 489 : un fichier riche en flux entre 256 et 489 niveaux diffère donc encore.
- Un texte contient un seul document, comme dans ansible-core : un second `---` arrête le chargement. Les limites ci-dessous comptent le texte entier.
- Les ancres et les alias d'un même texte copient au plus 1,000,000 de nœuds et 64 MiB de texte scalaire. Une copie est faite à chaque ancre et à chaque alias. ansible-core partage un nœud et ne copie rien : il charge donc un texte qui représente des milliards de nœuds et n'échoue que lorsqu'une tâche parcourt la valeur. Par exemple, 5,100 hôtes qui partagent par alias un mapping de 100 clés dépassent la limite de nœuds.
- Un texte `from_yaml` au-delà d'une limite fait échouer la tâche qui le lit, et les autres hôtes continuent. Un fichier au-delà d'une limite arrête l'exécution avant son début, comme le fait une erreur de syntaxe.
- Un fact imbriqué plus profondément que la limite YAML n'est pas écrit dans le cache de facts `jsonfile` : un `set_fact` avec `cacheable: true` affiche un avertissement et ne garde le fact que pour l'exécution. Le fichier contient deux niveaux de JSON pour chaque niveau de la valeur.

## Pas encore disponible

### Manques du rendu

- Une chaîne qui lit deux variables, dont l'une est chaînée à travers une troisième qui contient un bloc `raw`, est rendue une fois de plus que dans ansible-core.
- Un bloc `{% set x %}...{% endset %}` dont le corps affiche une expression fait du résultat une chaîne même quand le reste est une seule expression. Un bloc de texte seul garde le type.
- `dict` seul et `dict.fromkeys` seul ne sont pas un résultat, comme dans ansible-core (`Type 'type' is unsupported for variable storage.`), et s'affichent `<class 'dict'>` et `<built-in method fromkeys of type object>` à l'intérieur d'un texte.
- Un `datetime` venant de `now()` ne participe pas à l'arithmétique : `now() - now()` échoue donc, et il n'a pas les méthodes `date`, `time`, `replace`, `astimezone`, `utcoffset` et `timetuple`.

### Manques des fichiers modèles

- `template_host`, `template_uid`, `template_run_date` et `template_mtime` ne sont pas définis, contrairement à `template_path`, `template_fullpath` et `ansible_managed`.
- `line_statement_prefix` et `line_comment_prefix` dans une ligne `#jinja2:` sont refusés par leur nom. Le moteur de modèles de Volant et Jinja2 diffèrent sur une ligne vide après une instruction de ligne et sur le saut de ligne d'un commentaire de ligne.
- Un délimiteur de plus de 64 caractères et une ligne `#jinja2:` de plus de 16 KiB sont refusés avec une erreur qui nomme l'option. Les parenthèses dans une valeur `#jinja2:` s'imbriquent sur 200 niveaux, comme dans Python.
- Un `variable_end_string` ou un `block_end_string` qui commence par une espace est refusé par son nom. Un délimiteur vide est refusé aussi, là où Jinja2 le lit d'une façon qui dépend du modèle.
- Un `output_encoding` absent de ceux que liste la page [Templates](/fr/docs/variables/templating/), comme `koi8-r`, `shift_jis` ou `utf-7`, est refusé par son nom.

### Manques des bytes

- Une valeur bytes est une séquence d'entiers qui s'affiche `b'abc'`. Une tranche et `+` (`b[1:]`, `b + b`) donnent une liste d'entiers, là où Python donne des bytes.
- Une valeur bytes qu'une variable détient (`set_fact`, `vars:`) est stockée sous forme de texte : son `| length` compte des caractères, et `.decode()` échoue. Une boucle sur des éléments bytes affiche `(item=abc)` là où ansible-core affiche `(item=b'abc')`, parce qu'un élément de boucle est stocké sous forme de texte.
- Les méthodes de bytes autres que `decode` et `hex` (`upper`, `split`, `startswith`) échouent en citant leur nom.
- Une valeur bytes qui n'est pas de l'UTF-8 ne peut être ni un résultat ni être affichée dans un texte : ansible-core affiche `?` à sa place. `to_yaml`, `dict2items`, `flatten`, `hash`, `checksum`, `md5`, `sha1` et tout filtre qui s'exécute en Python refusent les bytes par leur nom.
- `encode` et `decode` refusent par leur nom les encodages autres que ceux listés plus haut (`cp1252`, `shift_jis`), `decode` de `utf-16` et `utf-32`, et les gestionnaires d'erreur `namereplace`, `surrogateescape` et `surrogatepass` sur `decode`.
