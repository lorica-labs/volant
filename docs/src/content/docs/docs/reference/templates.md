---
title: Template language reference
description: The Python methods, string rules, regular expressions and limits of Volant's Jinja2, with every difference from ansible-core.
---

This page lists what a template can call and where Volant differs from ansible-core. [Templating](/docs/variables/templating/) covers the everyday rules and the filters, tests and lookups Volant provides.

## Date and filler text

`now(utc=False, fmt=None)` gives the date and time on the controller, never on a managed host. It returns a `datetime` with no time zone, in UTC when `utc` is true. When `fmt` is set it returns the text `strftime(fmt)` gives instead, so `{{ now(utc=true, fmt='%Y%m%d') }}` is `20261019`. The `datetime` has the attributes `year`, `month`, `day`, `hour`, `minute`, `second`, `microsecond` and `tzinfo`, the methods `strftime`, `isoformat`, `timestamp`, `weekday` and `isoweekday`, and compares with another `datetime`. As a result or through `to_json` it prints as `2026-10-19T23:13:50.741151`. In text, through `string` or after `~`, it prints with a space instead of the `T`, as Python does. `strftime` always uses the C locale: day and month names are English and `%c` reads `Mon Oct 19 23:13:50 2026`. ansible-core uses the locale of the controller.

`lipsum(n=5, html=True, min=20, max=100)` gives `n` paragraphs of random filler text, as Jinja does. Each one has at least `min` and fewer than `max` words. With `html` true each paragraph sits in `<p>` tags and a newline separates them, otherwise a blank line does. One call makes at most 10 million words.

## Python methods

A template can call the methods Python gives to its values, as it can in ansible-core. A method that Volant does not run fails with its own name, so nothing is skipped without a message.

These `str` methods run, with the arguments and keywords Python takes and Python's wording for a wrong call (`must be str or None, not int`, `substring not found`):

| Group | Methods |
|---|---|
| Cut and join | `split`, `rsplit`, `splitlines`, `partition`, `rpartition`, `join`, `expandtabs` |
| Trim and pad | `strip`, `lstrip`, `rstrip`, `removeprefix`, `removesuffix`, `center`, `ljust`, `rjust`, `zfill` |
| Search | `find`, `rfind`, `index`, `rindex`, `count`, `startswith`, `endswith`, `replace` |
| Case | `upper`, `lower`, `title`, `capitalize`, `swapcase`, `casefold` |
| Test | `isalpha`, `isalnum`, `isdigit`, `isdecimal`, `isnumeric`, `isspace`, `islower`, `isupper`, `istitle`, `isascii`, `isidentifier`, `isprintable` |
| Other | `format`, `format_map`, `translate`, `maketrans`, `encode` |

Positions count characters, not bytes, and `start` and `end` take negative values. `split(maxsplit=1)` and the other keyword forms work. `startswith` and `endswith` take a text or a tuple of texts. A list is accepted as a tuple, because Volant's Jinja has no tuple type.

What differs from CPython:

- `casefold` is lower case plus `ß` to `ss`. It is not the full Unicode case folding.
- `isdigit`, `isalpha` and the other tests outside ASCII follow Rust's Unicode tables. `'½'.isdigit()` is true here and false in Python.
- A format field can read an item (`{0[a]}`, `{a[0]}`) but not an attribute (`{0.real}`).
- A width, a count or a precision that would make a method build more than 256 MiB of text (`'a'.ljust(10000000000)`, `'{:.300000000f}'.format(x)`) stops the task with an error that names the limit. Python builds the text until the machine runs out of memory. A number too large for an index gives Python's own error, `Python int too large to convert to C ssize_t`.
- `encode` makes a bytes value, and Volant runs only a few methods of bytes. See [Bytes](#bytes).
- A backslash escape such as `'\n'` inside a template string is read by the template engine. ansible-core 2.19 keeps the two characters. Pass a value in a variable when the text must hold a real newline.

### Lists and mappings

`list` and `dict` methods run too: `append`, `extend`, `insert`, `remove`, `pop`, `sort`, `reverse`, `index`, `count`, `copy` and `clear` on a list, and `get`, `keys`, `values`, `items`, `pop`, `popitem`, `setdefault`, `update`, `copy`, `clear` and `fromkeys` on a mapping. `fromkeys` is also a method of the `dict` name: `dict.fromkeys(['a', 'b'], 0)`. `keys()`, `values()` and `items()` give lists. `sort` takes `reverse`, and `key` only as `none`. The usual ways to write a change are `{% set _ = items.append(x) %}` and `{{ items.append(x) or '' }}`: the method changes the list and answers `none`.

A template changes a list or mapping in place, and other names for the same value see the change:

```jinja
{% set seen = [] %}
{% for host in groups['web'] %}{% set _ = seen.append(host) %}{% endfor %}
{% set alias = seen %}{% set _ = alias.append('db') %}
{{ seen }}   {# every web host, then db #}
```

This follows Python and ansible-core 2.19, which agree on these cases (each case is tested against ansible-core):

- A value read from a variable is a copy for the string being rendered. A play variable `x` that a template changes with `x.append(1)` is changed for that template only. The next task, the next field of the same task and the next template read it as it was. There is no change to a variable that outlasts the render.
- Inside one template, `b = a` names one list, and a change through `b` shows in `a`. `b = a | list` and `b = a.copy()` make a new list.
- A list or mapping inside another is reached by `x[0]`, `x.key` and `for item in x` as the same object, so `d.a.append(1)` changes `d`. A value stored by `append`, `extend`, `insert`, `update` or `setdefault` is a copy, and so are the values that `get`, `items`, `values` and `copy` answer.
- `d.setdefault('k', []).append(1)` changes `d`. `v = d.setdefault('k', [])` followed by `v.append(1)` changes `d` here, and does not in ansible-core.

What differs from ansible-core:

- A list or mapping that a template passes to a macro, `namespace()` or another call is the same object in Volant. ansible-core passes a copy, so a macro that changes its argument changes the caller's list only here.
- ansible-core 2.19 keeps the error of a call whose result is never read. `{% set _ = [1].remove(5) %}` is no error there. It is one here.
- A list that `a + b` or a slice `a[1:]` built cannot be changed: write `(a + b) | list`. The error says so.
- A list that a call has just built and that no variable holds, such as `(x | list).append(1)`, is changed as a copy: the call answers as Python does, and nothing else changes.
- A list that grows inside a `{% for %}` over itself is read up to its length at the start of the loop. One that `remove` or `pop` shrinks inside such a loop fails with an undefined value, where ansible-core goes on.
- A list that a filter answers is a copy, as in ansible-core, so `(items | first).append(1)` does not change `items`. A slice (`a[1:]`) shares the lists inside it with `a`.
- A list or mapping that is the item or the key of its own method is read as a copy of itself, and a mapping used as a key is an error (`unhashable type: 'dict'`), as in Python.
- `sort()` of a list that holds `nan` is an error here. ansible-core sorts it.
- `{% do %}` is accepted here and refused by ansible-core 2.19, which does not enable `jinja2.ext.do`. Use `{% set _ = ... %}` in a template that has to run on both.
- `items()` and `popitem()` give lists where Python gives tuples, so they print as `['a', 1]` and not `('a', 1)`.
- `sort(key=...)` with a function is not available.

Methods such as `.get()`, `.keys()`, `.values()` and `.items()` also work on other mapping-like values, such as `hostvars` and the loop variable.

### Bytes

`'text'.encode(encoding='utf-8', errors='strict')` makes a bytes value:

- `| length` counts the bytes. `b[0]`, `b[-1]`, `| first`, `| list` and `{% for %}` give integers.
- `b.decode(encoding, errors)` and `b.hex(sep, bytes_per_sep)` give text. `.encode('ascii', 'ignore').decode('ascii')` removes the characters that are not ASCII.
- `b64encode` reads the exact bytes. `b64decode`, `regex_replace`, `regex_search`, `regex_findall` and `quote` read them as UTF-8 text.
- A result that is bytes becomes its text, and so does `x {{ b }}`. `'x' ~ b`, `b | string` and `'%s' % b` give `b'abc'`, as in Python.
- `==`, `is sequence`, `is string` and `type_debug` (`bytes`) answer as in ansible-core. `to_json` and `to_nice_json` fail with `Object of type 'bytes' is not JSON serializable`.

`encode` knows `utf-8`, `ascii`, `latin-1`, `utf-16` and `utf-32` (`-le` and `-be` too), under the alias names Python reads (`UTF-8`, `utf8`, `iso-8859-1`). The error handlers are `strict`, `ignore`, `replace`, `backslashreplace` and `xmlcharrefreplace`. `decode` knows `utf-8`, `ascii` and `latin-1`, with `strict`, `ignore`, `replace` and `backslashreplace`. An error has Python's words: `'ascii' codec can't encode character '\xe9' in position 0: ordinal not in range(128)`.

### The `%` operator

On a text, `%` formats it the way Python does: `'%s-%d' % (name, 3)`, `'%(host)s:%(port)d' % item`, `'%5.2f%%' % pct`. The flags `-`, `+`, space, `0` and `#`, a width and a precision (each of them `*` to read it from the arguments), the length modifiers `h`, `l` and `L`, and the conversions `s`, `r`, `a`, `c`, `d`, `i`, `u`, `o`, `x`, `X`, `e`, `E`, `f`, `F`, `g`, `G` and `%` are read. A wrong call fails with Python's words: `not enough arguments for format string`, `not all arguments converted during string formatting`, `%d format: a real number is required, not str`.

Integers up to 128 bits that a template writes or computes are read exactly (`'%d' % 18446744073709551615`). An integer above 63 bits in a variable file reaches the template as text, which the YAML reader decides, not `%`. A width or a precision past 256 MiB of text is a task error that names the limit, as for the `str` methods, and one past what an index holds is Python's `width too big`.

On two numbers, `%` takes the sign of the divisor, as in Python: `7 % -3` is `-2`. A zero divisor is an error. The `format` filter is `%` with the arguments written out, and reads the same formats.

Volant has no tuple type, so the right side of `%` is read as a tuple when it is written out in the template, as `(a, b)`, `(a,)` or `['a', 'b']`. A list that a variable holds or a call answers is one argument, as it is in Python: `'%s' % items` prints the list. `'%s %s' % pair` with a two item list in `pair` is an error in ansible-core, and here. A tuple prints as a list: `'%s' % ((1, 2),)` is `[1, 2]` here and `(1, 2)` in ansible-core.

Two numbers written out in the template (`7 % -3`) are folded when the template is compiled, and a fold does not follow Python's sign rule or raise on a zero divisor. A number that comes from a variable does.

## Set filters

`difference`, `intersect` and `union` build a Python `set` in ansible-core, whose order is its own: small integers happen to come out sorted, and strings come out in an order that changes with `PYTHONHASHSEED` from one run to the next. Volant sorts the result when every element is an integer, which matches every integer case measured, and otherwise keeps the order in which the elements first appeared. On strings, that order is not guaranteed to match a given run of ansible-core.

## Strings that hold blocks

Volant renders any string that holds `{{`, `{%` or `{#`, in a variable, a fact, a task argument, a loop or an extra variable. It renders them as ansible-core does:

- `trim_blocks` is on and `lstrip_blocks` is off. The newline after a `{% ... %}` line is dropped, so a block scalar with a `{% for %}` loop gives one line for each pass and none for the tags.
- A result made of one `{{ }}` and nothing else keeps its type, even when statements or comments surround it. `{% set a = [1, 2] %}{{ a }}` is a list. A result made of text, or of several expressions, is a string.
- A result with no output at all is `none`. `{% if false %}x{% endif %}` and `{# note #}` are `none`, not an empty string.
- One newline at the end of the string is not part of the result when the result is not a string: `"{{ [1, 2] }}\n"` is a list. `{{-` and `-}}` strip the whitespace around the tag, so `{{- x -}}` with a newline after it is also one value, and the `-` is never a minus sign.
- `{#` starts a comment. A comment that opens at the very end of the string and never closes is accepted and means nothing. Anything else that is unterminated is a syntax error. To write `{#` as text, use `{{ '{#' }}`: a result that holds `{#` is not rendered again, and neither is a stored variable or fact whose `{#` has no `#}` after it.
- A string such as `echo ${#items[@]}` fails with a syntax error, as it does in ansible-core. Wrap it in `{% raw %}` or mark it `!unsafe`. ansible-core refuses a `command` or `shell` task that holds one while it loads the tasks, with exit code 4. Volant refuses it when the task runs, with exit code 2, so the tasks before it run first.

A registered result and any other text from a managed host stays text: it is never rendered. See [Trusted and untrusted values](#trusted-and-untrusted-values). A value under `!unsafe` is data and is never templated. A `!vault` value is decrypted, see [Vault](/docs/variables/vault/).

## Regular expressions

`regex_replace`, `regex_search`, `regex_findall` and the tests `match`, `search` and `regex` read Python `re` patterns. Volant rewrites the pattern for the Rust `regex` crate and walks the matches the way Python 3.7 and later does. Group handling, empty matches, replacement strings (`\1`, `\g<name>`, a literal `$`) and the arguments, including the flags by position, follow ansible-core.

Some Python syntax has no counterpart in the Rust crate. A pattern that uses it fails with an error that names the construct, where ansible-core runs it:

- possessive quantifiers (`a++`) and conditional groups (`(?(1)a|b)`)
- `\N{name}`, the flags `(?a)` and `(?L)`, and `(?x:` as a scoped flag
- a lazy repeat or an empty alternative before another one (`a*?`, `|a`) when an empty match needs a longer one tried at the same place
- `$` in a pattern without `multiline`, on a text that has more than one line and ends with a newline

Two differences stay. A match with several groups is a list of lists here and a list of tuples in ansible-core, which shows only when the value is printed as text. Volant's template engine reads `'\\'` in a string literal as one backslash, where ansible-core keeps two.

## Literal braces

Text that a `raw` block writes, or a string such as `'{{'`, is literal: it is not rendered again as an argument, in a variable, in a `set_fact` value or in a loop item. A variable that holds a `raw` block is read to its end first, so `a: "{{ b }}"` with `b: "{% raw %}{{ x }}{% endraw %}"` gives `{{ x }}`.

Braces that an expression builds are literal too. With `x: a`, `{{ x ~ '{{ 1 + 1 }}' }}` gives `a{{ 1 + 1 }}`, `{{ x | replace('a', '{{ 7 * 6 }}') }}` gives `{{ 7 * 6 }}`, and `{{ '{{ ' ~ n ~ '_port }}' }}` gives `{{ web_port }}` whatever `web_port` holds. A variable that holds a template is rendered when it is read, so a chain such as `a: "{{ b }}"` with `b: "{{ c }}"` gives the value of `c`. A result is rendered again only when a read gave it a template that was not rendered yet: `hostvars['other']['name']` for a variable that holds one, or a variable left as written by the rule above. A text that joins such a read to braces it builds is then rendered as a whole.

## Filters that run in Python

[Templating](/docs/variables/templating/#filters-tests-and-lookups-that-need-python) says which names need Python. These are the details.

A name in an `import_playbook` line goes through Python too, rendered with the variables of `-e`.

A Python plugin that raises an exception fails with ansible-core's chain: the plugin's sentence in one block, the exception's message in the next. A filter or test that raises prints the same chain in a task argument, in a `when` and in a `template` file.

Volant renders every argument first and sends the result as data. Python does not template it again, so a value that came from a managed host and still holds `{{ ... }}` stays plain text. Values marked `!unsafe` arrive as data too.

A lookup that runs in Python also receives the play's variables. Volant reads the plugin's source and names the variables it reads: the literal keys it looks up in `variables`, and the ones its settings name. A plugin that lists variables, such as `community.general.merge_variables`, gets all of them. Only such a plugin pays for sending every variable of the host.

Each of these fails the task that made the call, with the message of the Python process in the task's error:

- The Python process exits or is killed during a render.
- A request or an answer is larger than the frame limit.
- A plugin answers bytes that are not UTF-8.
- The Python process answers for another request.

The next call in the run that needs Python fails the same way instead of waiting.

## Trusted and untrusted values
A template you wrote is code. A value a managed host sent back is data. Volant keeps the two apart the way ansible-core 2.19 does, so a string a host controls never runs as a template on the controller.

:::caution
This is a security boundary. If you find a way for host data to be evaluated as a template on the controller, report it privately as described in the [security policy](https://github.com/lorica-labs/volant/blob/main/SECURITY.md).
:::


### Which values are untrusted

| Untrusted, because a host or the run produced it | Trusted, because the playbook carries it |
|---|---|
| `register` results and module results | play and task `vars` |
| gathered facts | `--extra-vars` |
| `set_fact` values | inventory variables |
| `lookup('file')`, `lookup('pipe')`, `lookup('env')`, `lookup('template')` | `vars_files` and `include_vars` |

`!unsafe` marks a value of the playbook as data. It is accepted on a value and on a mapping key, in a play, in `group_vars`, in an extra-vars file and in a YAML inventory. The filter that the YAML loader uses for the mark is hidden from playbooks: calling it fails with `No filter named 'volant_unsafe'`.

### What changes for an untrusted value

An untrusted value renders once. If the result looks like a template again, it stays as text and is not rendered a second time, which is what Ansible does:

```yaml
# Say someone wrote {{ lookup('pipe', 'id') }} into /etc/motd on the host.
- command: cat /etc/motd
  register: motd

- debug:
    msg: "{{ motd.stdout }}"   # prints the braces as text, runs nothing
```

`debug: var:` evaluates an expression. When the name itself came from a host, the task fails with ansible-core's own message:

```text
Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.
```

Writing the name yourself, as in `debug: var: result.stdout`, works as usual. It is `debug: var: "{{ from_a_host }}"` that stops.

Conditions follow the same rule. A `when: "{{ ... }}"` that renders to a string you wrote, such as `"1 == 1"`, is evaluated once more as a condition. If the render read something a host sent, the string is never compiled, and the task fails with the same `Encountered untrusted template or expression.` An `assert` whose `that` is templated works the same way.

A `lookup('template')` result is data too. A template holding `{{ '{{ 1 + 1 }}' }}` gives the text `{{ 1 + 1 }}`, not `2`, under both engines.

A `delegate_to` that renders from a host value names a host with that text, whatever it holds. The text is the delegate's `inventory_hostname` and `inventory_hostname_short`, and nothing renders it, even when a variable of your task puts braces next to it. The same holds for a name that holds braces wherever it came from, and for a delegate that is not a host of the inventory, such as one `add_host` made. Over ssh the task fails with `UNREACHABLE` and prints the text, as in ansible-core. A delegate that connects locally runs as usual.

A path that a managed host names is a separate case, because the controller reads or writes it. By default the `src` of `copy`, `template`, `script`, `unarchive` and `assemble` and the `dest` of `fetch` behave as in ansible-core. Two settings tighten them: [`host_named_paths` for a source file](/docs/internals/action-plugins/#a-source-file-a-managed-host-named) and [`host_named_dest_dirs` for a `dest`](/docs/internals/action-plugins/#a-dest-a-managed-host-named).

### How fine-grained it is

Volant tracks trust per variable, not per string. Ansible tags each string object, so a trusted string keeps its trust through a template and loses it only when an expression builds a new string from it. In Volant, a value is untrusted when the render that produced it read something a host contributed. The answer is the same wherever a host took part. Volant is more permissive on a value the playbook computed from its own text.

A managed host also cannot change its own connection settings through the facts a module returns. Volant strips the names ansible-core strips, such as `ansible_host`, `ansible_user` or `ansible_ssh_common_args`, plus `ansible_remote_tmp`, and prints the same warning.

## Limits on YAML
Volant refuses a YAML text that it cannot load without risk. The error names the limit. A playbook, a variable file, an inventory and the text a `from_yaml` filter reads all follow the same limits.

- A document nests collections at most 489 levels deep. ansible-core loads a file down to 489 levels and stops it at 490 with `maximum recursion depth exceeded`, and Volant gives the same message. An alias counts as the levels of the node it names. A flow collection stops earlier, at 255 levels, because the scanner counts them in a `u8`. ansible-core loads a flow collection down to 489, so a flow-heavy file between 256 and 489 levels still differs.
- A text holds one document, as in ansible-core: a second `---` stops the load. The limits below count the whole text.
- The anchors and aliases of one text copy at most 1,000,000 nodes and 64 MiB of scalar text. A copy is made at each anchor and at each alias. ansible-core shares a node and copies nothing, so it loads a text that stands for billions of nodes and fails only when a task walks the value. For example, 5,100 hosts that share a mapping of 100 keys by alias go over the node limit.
- A `from_yaml` text over a limit fails the task that reads it, and the other hosts go on. A file over a limit stops the run before it starts, as a syntax error does.
- A fact nested deeper than the YAML limit is not written to the `jsonfile` fact cache: a `set_fact` with `cacheable: true` prints a warning and keeps the fact for the run only. The file holds two levels of JSON for each level of the value.

## Not there yet

### Rendering gaps

- A string that reads two variables, one of them chained through a third that holds a `raw` block, is rendered once more than in ansible-core.
- A `{% set x %}...{% endset %}` block whose body prints an expression makes the result a string even when the rest is one expression. A block of text alone keeps the type.
- `dict` alone and `dict.fromkeys` alone are no result, as in ansible-core (`Type 'type' is unsupported for variable storage.`), and print as `<class 'dict'>` and `<built-in method fromkeys of type object>` inside a text.
- A `datetime` from `now()` takes part in `+` and `-` with another `datetime` or a `timedelta` and in no other arithmetic, and it lacks the methods `date`, `time`, `astimezone` and `timetuple`.

### Template file gaps

- `template_run_date` and `template_mtime` are text in Python's `str` of a `datetime`, not `datetime` values: they print the same, and `type_debug` says `str`.
- `line_statement_prefix` and `line_comment_prefix` in a `#jinja2:` line are refused by name. Volant's template engine and Jinja2 differ on a blank line after a line statement and on the newline of a line comment.
- A delimiter longer than 64 characters and a `#jinja2:` line longer than 16 KiB are refused with an error that names the option. Parentheses in a `#jinja2:` value nest to 200 levels, as in Python.
- A `variable_end_string` or `block_end_string` that begins with whitespace is refused by name. An empty delimiter is refused too, where Jinja2 reads it in a way that depends on the template.
- An `output_encoding` outside the ones [Templating](/docs/variables/templating/) lists, such as `koi8-r`, `shift_jis` or `utf-7`, is refused by name.

### Gaps in bytes

- A bytes value is a sequence of integers that prints as `b'abc'`. A slice and `+` (`b[1:]`, `b + b`) give a list of integers, where Python gives bytes.
- A bytes value that a variable holds (`set_fact`, `vars:`) is stored as its text: its `| length` counts characters, and `.decode()` fails. A loop over bytes items shows `(item=abc)` where ansible-core shows `(item=b'abc')`, because a loop item is stored as text.
- The methods of bytes other than `decode` and `hex` (`upper`, `split`, `startswith`) fail with their name.
- A bytes value that is not UTF-8 cannot be a result or be printed in text: ansible-core prints `?` in their place. `to_yaml`, `dict2items`, `flatten`, `hash`, `checksum`, `md5`, `sha1` and every filter that runs in Python refuse bytes by name.
- `encode` and `decode` refuse by name the encodings other than the ones listed above (`cp1252`, `shift_jis`), `decode` of `utf-16` and `utf-32`, and the error handlers `namereplace`, `surrogateescape` and `surrogatepass` on `decode`.
