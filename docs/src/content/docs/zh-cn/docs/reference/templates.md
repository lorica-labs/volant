---
sourceHash: d61771341944
title: 模板语言参考
description: Volant 的 Jinja2 所支持的 Python 方法、字符串规则、正则表达式和限制，以及与 ansible-core 的每一处差异。
---

本页列出模板可以调用什么，以及 Volant 在哪些地方与 ansible-core 不同。[模板](/zh-cn/docs/variables/templating/)介绍日常规则，以及 Volant 提供的过滤器、测试器和 lookup。

## 日期与占位文本

`now(utc=False, fmt=None)` 返回控制节点上的日期和时间，绝不是受管主机上的。它返回一个不带时区的 `datetime`，当 `utc` 为 true 时是 UTC。设置了 `fmt` 时，它返回 `strftime(fmt)` 得到的文本，所以 `{{ now(utc=true, fmt='%Y%m%d') }}` 是 `20261019`。`datetime` 具有属性 `year`、`month`、`day`、`hour`、`minute`、`second`、`microsecond` 和 `tzinfo`，具有方法 `strftime`、`isoformat`、`timestamp`、`weekday` 和 `isoweekday`，并且可以与另一个 `datetime` 比较。作为结果，或通过 `to_json` 输出时，它打印为 `2026-10-19T23:13:50.741151`。在文本中，通过 `string` 或在 `~` 之后，它与 Python 一样用空格代替 `T` 打印。`strftime` 始终使用 C locale：日期和月份名称是英文，`%c` 输出 `Mon Oct 19 23:13:50 2026`。ansible-core 则使用控制节点的 locale。

`lipsum(n=5, html=True, min=20, max=100)` 与 Jinja 一样，返回 `n` 段随机的占位文本。每段至少有 `min` 个词，少于 `max` 个词。`html` 为 true 时，每段放在 `<p>` 标签中，段与段之间用换行分隔，否则用空行分隔。一次调用最多生成 1000 万个词。

## Python 方法

模板可以调用 Python 为其值提供的方法，就像在 ansible-core 中一样。Volant 不支持运行的方法会以它自己的名字报错，所以不会有任何东西被无声地跳过。

下面这些 `str` 方法可以运行，接受 Python 接受的参数和关键字，调用错误时使用 Python 的措辞（`must be str or None, not int`、`substring not found`）：

| 分组 | 方法 |
|---|---|
| 切分与连接 | `split`、`rsplit`、`splitlines`、`partition`、`rpartition`、`join`、`expandtabs` |
| 裁剪与填充 | `strip`、`lstrip`、`rstrip`、`removeprefix`、`removesuffix`、`center`、`ljust`、`rjust`、`zfill` |
| 查找 | `find`、`rfind`、`index`、`rindex`、`count`、`startswith`、`endswith`、`replace` |
| 大小写 | `upper`、`lower`、`title`、`capitalize`、`swapcase`、`casefold` |
| 判断 | `isalpha`、`isalnum`、`isdigit`、`isdecimal`、`isnumeric`、`isspace`、`islower`、`isupper`、`istitle`、`isascii`、`isidentifier`、`isprintable` |
| 其他 | `format`、`format_map`、`translate`、`maketrans`、`encode` |

位置按字符计数，而不是按字节，`start` 和 `end` 可以取负值。`split(maxsplit=1)` 和其他关键字形式都可用。`startswith` 和 `endswith` 接受一个文本或一个文本元组。列表也被当作元组接受，因为 Volant 的 Jinja 没有元组类型。

与 CPython 的差异：

- `casefold` 是转小写再加上 `ß` 到 `ss` 的转换。它不是完整的 Unicode 大小写折叠。
- `isdigit`、`isalpha` 和其他判断方法在 ASCII 之外遵循 Rust 的 Unicode 表。`'½'.isdigit()` 在这里为 true，在 Python 中为 false。
- 格式字段可以读取一个条目（`{0[a]}`、`{a[0]}`），但不能读取属性（`{0.real}`）。
- 会让方法构建出超过 256 MiB 文本的宽度、计数或精度（`'a'.ljust(10000000000)`、`'{:.300000000f}'.format(x)`），会让 task 以点名该限制的错误停止。Python 会一直构建文本，直到机器耗尽内存。对索引来说过大的数字，会得到 Python 自己的错误 `Python int too large to convert to C ssize_t`。
- `encode` 生成 bytes 值，而 Volant 只运行 bytes 的少数几个方法。见[字节](#字节)。
- 模板字符串中的反斜杠转义（例如 `'\n'`）由模板引擎解释。ansible-core 2.19 则保留这两个字符。当文本必须包含真正的换行符时，请通过变量传入。

### 列表与映射

`list` 和 `dict` 的方法也可以运行：列表上的 `append`、`extend`、`insert`、`remove`、`pop`、`sort`、`reverse`、`index`、`count`、`copy` 和 `clear`，以及映射上的 `get`、`keys`、`values`、`items`、`pop`、`popitem`、`setdefault`、`update`、`copy`、`clear` 和 `fromkeys`。`fromkeys` 也是 `dict` 这个名字的方法：`dict.fromkeys(['a', 'b'], 0)`。`keys()`、`values()` 和 `items()` 返回列表。`sort` 接受 `reverse`，而 `key` 只能是 `none`。写出修改的常见方式是 `{% set _ = items.append(x) %}` 和 `{{ items.append(x) or '' }}`：方法修改列表，并返回 `none`。

模板会就地修改列表或映射，同一个值的其他名字也会看到这个修改：

```jinja
{% set seen = [] %}
{% for host in groups['web'] %}{% set _ = seen.append(host) %}{% endfor %}
{% set alias = seen %}{% set _ = alias.append('db') %}
{{ seen }}   {# every web host, then db #}
```

这遵循 Python 和 ansible-core 2.19，它们在这些情形上一致（每种情形都对照 ansible-core 测试过）：

- 从变量读到的值，对正在渲染的字符串而言是一份副本。模板用 `x.append(1)` 修改的 play 变量 `x`，只对该模板有改动。下一个 task、同一个 task 的下一个字段和下一个模板读到的都是它原来的样子。不存在比这次渲染活得更久的变量修改。
- 在同一个模板内部，`b = a` 指向同一个列表，通过 `b` 的修改会在 `a` 中显示。`b = a | list` 和 `b = a.copy()` 会生成新列表。
- 嵌套在另一个里面的列表或映射，通过 `x[0]`、`x.key` 和 `for item in x` 访问到的是同一个对象，所以 `d.a.append(1)` 会修改 `d`。由 `append`、`extend`、`insert`、`update` 或 `setdefault` 存入的值是副本，`get`、`items`、`values` 和 `copy` 返回的值也是副本。
- `d.setdefault('k', []).append(1)` 会修改 `d`。`v = d.setdefault('k', [])` 后接 `v.append(1)`，在这里会修改 `d`，在 ansible-core 中则不会。

与 ansible-core 的差异：

- 模板传给宏、`namespace()` 或其他调用的列表或映射，在 Volant 中是同一个对象。ansible-core 传递的是副本，所以修改自己参数的宏，只有在这里才会修改调用方的列表。
- ansible-core 2.19 会保留结果从未被读取的调用所产生的错误。`{% set _ = [1].remove(5) %}` 在那里不是错误，在这里是。
- 由 `a + b` 或切片 `a[1:]` 构建的列表不能被修改：请写 `(a + b) | list`。错误消息会这样提示。
- 刚由调用构建、没有任何变量持有的列表，例如 `(x | list).append(1)`，是作为副本被修改的：调用像 Python 那样应答，其他东西都不会改变。
- 在对自身进行的 `{% for %}` 内部增长的列表，只会读到循环开始时的长度。在这样的循环内部被 `remove` 或 `pop` 缩短的列表，会因未定义的值而失败，而 ansible-core 会继续。
- 过滤器返回的列表是副本，与 ansible-core 一致，所以 `(items | first).append(1)` 不会修改 `items`。切片（`a[1:]`）与 `a` 共享其中的列表。
- 作为自身方法的条目或键的列表或映射，会被读成它自己的副本，用作键的映射是错误（`unhashable type: 'dict'`），与 Python 一致。
- 对含有 `nan` 的列表调用 `sort()`，在这里是错误。ansible-core 则会排序。
- `{% do %}` 在这里被接受，而 ansible-core 2.19 拒绝它，因为它没有启用 `jinja2.ext.do`。在必须同时在两个引擎上运行的模板中，请使用 `{% set _ = ... %}`。
- `items()` 和 `popitem()` 返回列表，而 Python 返回元组，所以它们打印为 `['a', 1]`，而不是 `('a', 1)`。
- 带函数的 `sort(key=...)` 不可用。

`.get()`、`.keys()`、`.values()` 和 `.items()` 这类方法，也适用于其他类似映射的值，例如 `hostvars` 和循环变量。

### 字节

`'text'.encode(encoding='utf-8', errors='strict')` 生成一个 bytes 值：

- `| length` 统计字节数。`b[0]`、`b[-1]`、`| first`、`| list` 和 `{% for %}` 得到整数。
- `b.decode(encoding, errors)` 和 `b.hex(sep, bytes_per_sep)` 得到文本。`.encode('ascii', 'ignore').decode('ascii')` 会删除不是 ASCII 的字符。
- `b64encode` 读取确切的字节。`b64decode`、`regex_replace`、`regex_search`、`regex_findall` 和 `quote` 把它们当作 UTF-8 文本读取。
- 结果如果是 bytes，就会变成它的文本，`x {{ b }}` 也一样。`'x' ~ b`、`b | string` 和 `'%s' % b` 得到 `b'abc'`，与 Python 一致。
- `==`、`is sequence`、`is string` 和 `type_debug`（`bytes`）的应答与 ansible-core 一致。`to_json` 和 `to_nice_json` 会失败，并给出 `Object of type 'bytes' is not JSON serializable`。

`encode` 认识 `utf-8`、`ascii`、`latin-1`、`utf-16` 和 `utf-32`（也包括 `-le` 和 `-be`），以及 Python 读取的别名（`UTF-8`、`utf8`、`iso-8859-1`）。错误处理方式有 `strict`、`ignore`、`replace`、`backslashreplace` 和 `xmlcharrefreplace`。`decode` 认识 `utf-8`、`ascii` 和 `latin-1`，错误处理方式有 `strict`、`ignore`、`replace` 和 `backslashreplace`。错误使用 Python 的措辞：`'ascii' codec can't encode character '\xe9' in position 0: ordinal not in range(128)`。

### `%` 运算符

作用于文本时，`%` 像 Python 那样格式化它：`'%s-%d' % (name, 3)`、`'%(host)s:%(port)d' % item`、`'%5.2f%%' % pct`。标志 `-`、`+`、空格、`0` 和 `#`，宽度和精度（二者都可以用 `*` 从参数中读取），长度修饰符 `h`、`l` 和 `L`，以及转换符 `s`、`r`、`a`、`c`、`d`、`i`、`u`、`o`、`x`、`X`、`e`、`E`、`f`、`F`、`g`、`G` 和 `%` 都可以读取。调用错误时使用 Python 的措辞：`not enough arguments for format string`、`not all arguments converted during string formatting`、`%d format: a real number is required, not str`。

模板写出或计算出的、最多 128 位的整数会被精确读取（`'%d' % 18446744073709551615`）。变量文件中超过 63 位的整数，会以文本形式到达模板，这由 YAML 读取器决定，而不是 `%`。超过 256 MiB 文本的宽度或精度是 task 错误，并点名该限制，与 `str` 方法一样，超过索引所能容纳范围的，则是 Python 的 `width too big`。

作用于两个数字时，`%` 的结果带除数的符号，与 Python 一致：`7 % -3` 是 `-2`。除数为零是错误。`format` 过滤器就是把参数写出来的 `%`，读取同样的格式。

Volant 没有元组类型，所以当 `%` 右侧在模板中直接写出时，如 `(a, b)`、`(a,)` 或 `['a', 'b']`，会被读作元组。变量持有的列表或调用返回的列表是一个参数，与 Python 一样：`'%s' % items` 打印这个列表。`pair` 中放着含两项的列表时，`'%s %s' % pair` 在 ansible-core 中是错误，在这里也是。元组打印为列表：`'%s' % ((1, 2),)` 在这里是 `[1, 2]`，在 ansible-core 中是 `(1, 2)`。

模板中直接写出的两个数字（`7 % -3`）会在编译模板时折叠，折叠既不遵循 Python 的符号规则，也不会在除数为零时报错。来自变量的数字则会。

## 集合过滤器

`difference`、`intersect` 和 `union` 在 ansible-core 中构建一个 Python `set`，其顺序由它自己决定：小整数恰好按排序后的顺序出现，而字符串的顺序会随 `PYTHONHASHSEED` 在每次运行之间变化。当每个元素都是整数时，Volant 会对结果排序，这与测过的每一种整数情形都一致，否则保持元素第一次出现的顺序。对于字符串，这个顺序不保证与某次 ansible-core 运行一致。

## 包含模板块的字符串

Volant 会渲染任何含有 `{{`、`{%` 或 `{#` 的字符串，不论它在变量、fact、task 参数、循环还是额外变量中。渲染方式与 ansible-core 相同：

- `trim_blocks` 开启，`lstrip_blocks` 关闭。`{% ... %}` 行之后的换行会被丢弃，所以带有 `{% for %}` 循环的块标量，每次循环产生一行，标签本身不产生行。
- 仅由一个 `{{ }}` 构成的结果会保持其类型，即使周围有语句或注释。`{% set a = [1, 2] %}{{ a }}` 是列表。由文本或多个表达式构成的结果是字符串。
- 完全没有输出的结果是 `none`。`{% if false %}x{% endif %}` 和 `{# note #}` 是 `none`，而不是空字符串。
- 当结果不是字符串时，字符串末尾的一个换行符不属于结果：`"{{ [1, 2] }}\n"` 是列表。`{{-` 和 `-}}` 会去除标签周围的空白，所以后面带换行的 `{{- x -}}` 也是单个值，这里的 `-` 从不是减号。
- `{#` 开始一段注释。恰好在字符串末尾打开且从不关闭的注释会被接受，并且没有任何含义。其他任何未终止的情形都是语法错误。要把 `{#` 当作文本写出，请使用 `{{ '{#' }}`：包含 `{#` 的结果不会再次渲染，其 `{#` 之后没有 `#}` 的已存变量或 fact 也不会。
- 像 `echo ${#items[@]}` 这样的字符串会因语法错误而失败，与 ansible-core 一致。请把它包在 `{% raw %}` 中，或标记为 `!unsafe`。ansible-core 在加载 task 时就会拒绝含有这类字符串的 `command` 或 `shell` task，退出码为 4。Volant 在该 task 运行时才拒绝它，退出码为 2，所以它之前的 task 会先运行。

注册的结果和来自受管主机的其他任何文本始终是文本：它们从不被渲染。见[可信值与不可信值](#可信值与不可信值)。带有 `!unsafe` 的值是数据，从不被当作模板。`!vault` 值会被解密，见 [Vault](/zh-cn/docs/variables/vault/)。

## 正则表达式

`regex_replace`、`regex_search`、`regex_findall`，以及测试器 `match`、`search` 和 `regex` 读取 Python 的 `re` 模式。Volant 会把模式改写为 Rust 的 `regex` crate 所用的形式，并像 Python 3.7 及更高版本那样遍历匹配。分组处理、空匹配、替换字符串（`\1`、`\g<name>`、字面的 `$`）以及参数（包括按位置给出的标志），都遵循 ansible-core。

Python 的一些语法在 Rust crate 中没有对应物。使用它们的模式会失败，错误消息会点名该构造，而 ansible-core 能运行它们：

- 前瞻和后顾：`(?=`、`(?!`、`(?<=`、`(?<!`
- 模式内部的反向引用：`\1`、`(?P=name)`
- 占有量词（`a++`）、原子组（`(?>`）和条件组（`(?(1)a|b)`）
- `\N{name}`、标志 `(?a)` 和 `(?L)`，以及作为限定范围标志的 `(?x:`
- 当空匹配需要在同一位置尝试更长的匹配时，懒惰重复或另一个分支之前的空分支（`a*?`、`|a`）
- 在没有 `multiline` 的模式中，用于含多行且以换行符结尾的文本的 `$`

有两处差异保留。有多个分组的匹配在这里是列表的列表，在 ansible-core 中是元组的列表，只有当值被打印为文本时才看得出来。Volant 的模板引擎把字符串字面量中的 `'\\'` 读作一个反斜杠，而 ansible-core 保留两个。

## 字面花括号

`raw` 块写出的文本，或像 `'{{'` 这样的字符串，是字面的：作为参数、在变量中、在 `set_fact` 的值中或在循环项中，它们都不会再次被渲染。持有 `raw` 块的变量会先被读到末尾，所以 `a: "{{ b }}"` 加上 `b: "{% raw %}{{ x }}{% endraw %}"` 得到 `{{ x }}`。

表达式构造出来的花括号同样是字面的。在 `x: a` 的情况下，`{{ x ~ '{{ 1 + 1 }}' }}` 得到 `a{{ 1 + 1 }}`，`{{ x | replace('a', '{{ 7 * 6 }}') }}` 得到 `{{ 7 * 6 }}`，`{{ '{{ ' ~ n ~ '_port }}' }}` 得到 `{{ web_port }}`，不论 `web_port` 的值是什么。持有模板的变量在被读取时渲染，所以 `a: "{{ b }}"` 加上 `b: "{{ c }}"` 这样的链条得到 `c` 的值。只有当某次读取给出了一个尚未渲染的模板时，结果才会再次渲染：对持有模板的变量使用 `hostvars['other']['name']`，或者按上面的规则保持原样的变量。把这样的读取与它自己构造的花括号连在一起的文本，随后会作为一个整体被渲染。

## 在 Python 中运行的过滤器

[模板](/zh-cn/docs/variables/templating/#需要-python-的过滤器测试器和-lookup)说明了哪些名字需要 Python。下面是细节。

`import_playbook` 行中的名字同样要经过 Python，并用 `-e` 的变量渲染。

抛出异常的 Python 插件会以 ansible-core 的链条失败：插件的那句话放在一个块里，异常的消息放在下一个块里。抛出异常的过滤器或测试器，在 task 参数、`when` 和 `template` 文件中会打印同样的链条。

Volant 会先渲染每个参数，并把结果作为数据发送。Python 不会再次对其进行模板处理，所以来自受管主机、仍然含有 `{{ ... }}` 的值会保持为纯文本。标记为 `!unsafe` 的值同样以数据形式到达。

在 Python 中运行的 lookup 也会收到 play 的变量。Volant 读取插件的源码，并点出它读取的变量：它在 `variables` 中查找的字面键，以及它的设置所指名的变量。会列出变量的插件，例如 `community.general.merge_variables`，会得到全部变量。只有这样的插件才需要为发送主机的每个变量付出代价。

下面每一种情况都会让发起调用的 task 失败，Python 进程的消息会出现在 task 的错误中：

- Python 进程在渲染期间退出或被杀死。
- 请求或应答超过帧大小限制。
- 插件应答的 bytes 不是 UTF-8。
- Python 进程应答的是另一个请求。

运行中下一个需要 Python 的调用会以同样的方式失败，而不是等待。

## 可信值与不可信值

你写的模板是代码。受管主机送回的值是数据。Volant 像 ansible-core 2.19 那样把这两者分开，所以主机能控制的字符串永远不会作为模板在控制节点上运行。

:::caution
这是一条安全边界。如果你发现了让主机数据在控制节点上被当作模板求值的办法，请按[安全策略](https://github.com/lorica-labs/volant/blob/main/SECURITY.md)中的说明私下报告。
:::

### 哪些值不可信

| 不可信，因为由主机或运行本身产生 | 可信，因为由 playbook 携带 |
|---|---|
| `register` 结果和模块结果 | play 和 task 的 `vars` |
| 收集到的 facts | `--extra-vars` |
| `set_fact` 的值 | inventory 变量 |
| `lookup('file')`、`lookup('pipe')`、`lookup('env')`、`lookup('template')` | `vars_files` 和 `include_vars` |

`!unsafe` 把 playbook 的一个值标记为数据。它可用于值和映射的键，可用于 play、`group_vars`、额外变量文件和 YAML inventory。YAML 加载器用于这个标记的过滤器对 playbook 是隐藏的：调用它会失败，并给出 `No filter named 'volant_unsafe'`。

### 不可信值会有什么变化

不可信值只渲染一次。如果结果看起来又像模板，它会保持为文本，不会被第二次渲染，这与 Ansible 的做法相同：

```yaml
# Say someone wrote {{ lookup('pipe', 'id') }} into /etc/motd on the host.
- command: cat /etc/motd
  register: motd

- debug:
    msg: "{{ motd.stdout }}"   # prints the braces as text, runs nothing
```

`debug: var:` 会对一个表达式求值。当名字本身来自主机时，task 会失败，并给出 ansible-core 自己的消息：

```text
Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.
```

像 `debug: var: result.stdout` 这样由你自己写出名字则照常工作。会被拦下的是 `debug: var: "{{ from_a_host }}"`。

条件遵循同样的规则。渲染为你自己写的字符串（例如 `"1 == 1"`）的 `when: "{{ ... }}"`，会被再作为条件求值一次。如果这次渲染读取了主机发来的内容，这个字符串就绝不会被编译，task 会失败，并给出同样的 `Encountered untrusted template or expression.`。`that` 经过模板处理的 `assert` 也以同样的方式工作。

`lookup('template')` 的结果同样是数据。包含 `{{ '{{ 1 + 1 }}' }}` 的模板，在两个引擎下都得到文本 `{{ 1 + 1 }}`，而不是 `2`。

由主机值渲染出的 `delegate_to` 会用该文本指名一台主机，不论其内容是什么。这段文本就是委派目标的 `inventory_hostname` 和 `inventory_hostname_short`，没有任何东西会渲染它，即使你的 task 的某个变量在它旁边放了花括号。无论来源如何，含有花括号的名字，以及不属于 inventory 的委派目标（例如由 `add_host` 创建的），都是如此。通过 ssh 时，task 会以 `UNREACHABLE` 失败并打印该文本，与 ansible-core 一致。在本地连接的委派目标则照常运行。

受管主机指定的路径是另一种情形，因为读取或写入它的是控制节点。默认情况下，`copy`、`template`、`script`、`unarchive` 和 `assemble` 的 `src`，以及 `fetch` 的 `dest`，行为与 ansible-core 相同。有两个设置可以收紧它们：针对源文件的 [`host_named_paths`](/zh-cn/docs/internals/action-plugins/#受管主机指定的源文件)，以及针对 `dest` 的 [`host_named_dest_dirs`](/zh-cn/docs/internals/action-plugins/#受管主机指定的-dest)。

### 粒度有多细

Volant 按变量而不是按字符串跟踪信任。Ansible 给每个字符串对象打标记，所以可信字符串在经过模板后仍然可信，只有当表达式用它构建出新字符串时才失去信任。在 Volant 中，如果产生某个值的那次渲染读取了主机提供的内容，这个值就是不可信的。凡是主机参与了的地方，结论都相同。对于 playbook 根据自己的文本计算出的值，Volant 更宽松。

受管主机也无法通过模块返回的 facts 改变自己的连接设置。Volant 会剥除 ansible-core 剥除的那些名字，例如 `ansible_host`、`ansible_user` 或 `ansible_ssh_common_args`，另外还有 `ansible_remote_tmp`，并打印同样的警告。

## YAML 的限制

Volant 会拒绝无法在没有风险的前提下加载的 YAML 文本。错误消息会点名具体的限制。playbook、变量文件、inventory，以及 `from_yaml` 过滤器读取的文本，都遵循同样的限制。

- 一个文档嵌套集合最多 489 层。ansible-core 最深可加载 489 层的文件，到 490 层就会以 `maximum recursion depth exceeded` 停止，Volant 给出同样的消息。别名按它所指名节点的层数计算。流式集合更早停止，在 255 层，因为扫描器用 `u8` 计数。ansible-core 最深可以加载 489 层的流式集合，所以流式集合很多、层数在 256 到 489 之间的文件仍然存在差异。
- 与 ansible-core 一样，一段文本只含一个文档：第二个 `---` 会让加载停止。下面的限制统计整段文本。
- 一段文本中的锚点和别名最多复制 1,000,000 个节点和 64 MiB 的标量文本。复制发生在每个锚点和每个别名处。ansible-core 共享节点而不复制任何东西，所以它能加载一段代表数十亿节点的文本，直到某个 task 遍历该值时才失败。例如，5,100 台主机通过别名共享一个有 100 个键的映射，就会超过节点限制。
- 超过限制的 `from_yaml` 文本会让读取它的 task 失败，其他主机继续。超过限制的文件会让运行在开始前停止，与语法错误一样。
- 嵌套深度超过 YAML 限制的 fact 不会写入 `jsonfile` facts 缓存：带有 `cacheable: true` 的 `set_fact` 会打印警告，并且只在本次运行中保留该 fact。值每深一层，文件中就多两层 JSON。

## 尚未支持

### 渲染方面的缺口

- 读取两个变量的字符串，若其中一个是经由第三个持有 `raw` 块的变量链接而来，就会比 ansible-core 多渲染一次。
- 主体中打印了表达式的 `{% set x %}...{% endset %}` 块会让结果成为字符串，即使其余部分是一个表达式。只含文本的块会保留类型。
- 单独的 `dict` 和单独的 `dict.fromkeys` 不是结果，与 ansible-core 一致（`Type 'type' is unsupported for variable storage.`），在文本中打印为 `<class 'dict'>` 和 `<built-in method fromkeys of type object>`。
- `now()` 返回的 `datetime` 不参与算术运算，所以 `now() - now()` 会失败，并且它缺少方法 `date`、`time`、`replace`、`astimezone`、`utcoffset` 和 `timetuple`。

### 模板文件方面的缺口

- `template_host`、`template_uid`、`template_run_date` 和 `template_mtime` 没有设置，与之不同的是 `template_path`、`template_fullpath` 和 `ansible_managed`。
- `#jinja2:` 行中的 `line_statement_prefix` 和 `line_comment_prefix` 会按名字被拒绝。Volant 的模板引擎与 Jinja2 在行语句之后的空行以及行注释的换行符上存在差异。
- 超过 64 个字符的定界符和超过 16 KiB 的 `#jinja2:` 行会被拒绝，错误消息会点名该选项。`#jinja2:` 值中的括号最多嵌套 200 层，与 Python 一致。
- 以空白开头的 `variable_end_string` 或 `block_end_string` 会按名字被拒绝。空的定界符同样被拒绝，而 Jinja2 对它的解读取决于模板。
- 不在[模板](/zh-cn/docs/variables/templating/)所列范围内的 `output_encoding`，例如 `koi8-r`、`shift_jis` 或 `utf-7`，会按名字被拒绝。

### bytes 方面的缺口

- bytes 值是一个整数序列，打印为 `b'abc'`。切片和 `+`（`b[1:]`、`b + b`）得到整数列表，而 Python 得到 bytes。
- 变量持有的 bytes 值（`set_fact`、`vars:`）以它的文本形式存储：它的 `| length` 统计字符，`.decode()` 会失败。对 bytes 项的循环显示 `(item=abc)`，而 ansible-core 显示 `(item=b'abc')`，因为循环项是以文本存储的。
- bytes 除 `decode` 和 `hex` 之外的方法（`upper`、`split`、`startswith`）会以其名字失败。
- 不是 UTF-8 的 bytes 值不能作为结果，也不能在文本中打印：ansible-core 会在它们的位置打印 `?`。`to_yaml`、`dict2items`、`flatten`、`hash`、`checksum`、`md5`、`sha1`，以及每个在 Python 中运行的过滤器，都会按名字拒绝 bytes。
- `encode` 和 `decode` 会按名字拒绝上面列出的编码以外的编码（`cp1252`、`shift_jis`）、`utf-16` 和 `utf-32` 的 `decode`，以及 `decode` 上的错误处理方式 `namereplace`、`surrogateescape` 和 `surrogatepass`。
