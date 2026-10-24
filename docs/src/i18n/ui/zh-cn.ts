import type { Deep, Strings } from './en';

// Translation of en.ts. A key left out shows in English.
export const zhCn: Deep<Strings> = {
  nav: {
    docs: '文档',
    performance: '性能',
    compatibility: '兼容性',
    community: '社区',
    blog: '博客',
  },
  header: {
    home: 'Volant，首页',
    install: '安装',
    menu: '菜单',
    releaseNotes: '发布说明',
    github: 'GitHub 上的 Volant',
    language: '语言',
    skip: '跳到正文',
  },
  footer: {
    about: '用 Rust 编写的 Ansible playbook 引擎。以 GPL-3.0-or-later 开源。',
    use: '使用',
    reference: '参考',
    project: '项目',
    source: '源码',
    installation: '安装',
    quickstart: '快速上手',
    willItRun: '我的 playbook 能运行吗？',
    whenItFails: '运行失败时',
    leaves: 'Volant 会在你的机器上留下什么',
    uninstall: '卸载',
    cli: '命令行',
    configuration: '配置',
    matrix: '支持矩阵',
    changelog: '更新日志',
    roadmap: '路线图',
    contributing: '参与贡献',
    security: '安全策略',
    license: '许可证',
    telemetry: 'Volant 不收集任何遥测数据。',
    name: '<i>Volant</i> 是法语词，意思是方向盘。',
  },
  copy: { copy: '复制', copied: '已复制', manual: '选中并复制', label: '复制命令' },
  lookup: {
    label: '是否支持？查询模块、collection、过滤器或选项',
    button: '查询',
  },
  home: {
    title: 'Volant：由 Rust 编写的高速引擎运行你现有的 Ansible playbook',
    description:
      'Volant 读取你现有的 playbook、role 和 inventory，通过 SSH 在 Linux 主机上运行，比 ansible-playbook 快得多。',
    heading: '你的 Ansible playbook，由一个用&nbsp;Rust&nbsp;编写的高速引擎运行。',
    lede: 'Volant 读取你现有的 playbook、role 和 inventory，通过 SSH 运行。无需转换。',
    platforms: '支持 Linux 和 macOS，x86_64 与 arm64。不需要安装 Ansible。尚不支持 Windows。',
    quickstart: '快速上手',
    source: 'GitHub 源码',
    badge: 'pre-alpha',
    status:
      '它能运行真实的 playbook，但还不是所有 playbook。请在可以承担改动的主机上试用，并查看<a class="text-link" href="/docs/#what-volant-leaves-on-your-machines">它会在你的机器上留下什么</a>。',
    runTitle: '一次运行，五台主机。',
    runText:
      '一次真实的 Volant 运行，重新播放。轮子的每根辐条代表一台主机，颜色就是它返回的结果，采用 ansible-playbook 使用的配色。',
    howTitle: '工作方式。',
    howText:
      'ansible-playbook 每执行一个 task，都要在主机上付出启动新 Python 解释器的代价。Volant 每次运行只付出一次，其余时间保持一条连接。',
    everyTask: '每个 task，每台主机',
    slowSteps: ['构建模块载荷', '通过 SSH 发送', '启动 Python', '导入 module_utils', '运行模块'],
    oncePerRun: '每次运行一次',
    onceSteps: ['上传一个小型 agent，缓存在主机上', '每台主机打开一条 SSH 连接'],
    then: '之后',
    forEveryTask: '每个 task',
    fastSteps: ['通过已打开的连接发送 task', '用 Rust 直接应答，或 fork 一个已预热的 Python'],
    howNote:
      '常见模块，如 <code>apt</code>、<code>systemd</code>、<code>file</code>、<code>lineinfile</code>，以及 facts 收集，在 task 的参数允许时由 agent 用 Rust 应答。其他模块都在 Python 中运行，由一个只加载一次 import 的服务进程执行。<a class="text-link" href="/docs/internals/architecture/">引擎是如何构建的</a>',
    filesTitle: '同样的文件，同样的命令。',
    filesText:
      '<code>volant playbook</code> 接受 <code>ansible-playbook</code> 的选项，并打印你熟悉的输出，直到汇总。你的项目里已有的一切都按原样读取。',
    filesLink: '运行你的第一个 playbook',
    files: [
      ['ansible.cfg', '从相同位置读取，优先级相同'],
      ['Inventory', 'INI、YAML、TOML、脚本和 inventory 插件'],
      ['group_vars 和 host_vars', '按 Ansible 的顺序叠加'],
      ['Vault', '经 vault 加密的文件和值，--ask-vault-pass 与 --vault-id'],
      ['Role 和 collection', '依赖、参数规范和已安装的 collection'],
      ['命令行', 'ansible-playbook 的选项，包括 --check 和 --diff'],
    ],
    refusalTitle: '碰到主机之前，它会先拒绝。',
    refusalText:
      'Volant 在连接之前检查整个 playbook。这个版本无法运行的内容，大多会在这里被点名，并给出所在的 play 或 task，运行随后以退出码 4 停止。任何主机上都没有发生改动。',
    refusalCheck:
      '<code>--syntax-check</code> 和列表输出不会因为不支持的内容而停止，<code>--check</code> 加 <code>--diff</code> 会显示真实运行将改动什么。<a class="text-link" href="/docs/start/compatibility/">检查一个项目</a>',
    refusalCaption: 'Volant 0.1.0-alpha.10 的真实输出，针对一个请求了本版本没有的策略的 play。',
    runtime: [
      ['获取什么', '它自己的 Python 和 ansible-core，约 155 MB，在 playbook 第一次需要 Python 时下载'],
      ['如何校验', '每个文件都对照发布版中固定的哈希'],
      ['存放位置', '<code>~/.local/share/volant/runtime</code>，与系统 Python 分开'],
      ['Collection', '<code>volant runtime install --collections community.general</code>'],
      ['没有网络时', '在一台机器上运行 <code>volant runtime pack</code>，在另一台上运行 <code>install --from</code>'],
    ],
    runtimeTitle: '无需安装 Ansible。',
    runtimeText:
      '二进制文件自带它上传到主机的 agent。playbook 需要 Python 模块时，Volant 会安装自己的运行时，并与机器上的其他部分隔离。',
    runtimeLink: '托管运行时',
    faster: '在 {workload} 上更快，对比启用 pipelining 的 ansible-core',
    coreShare: '的 ansible-core 关键字、选项和插件（共 {total} 个）现已可运行',
    blogTitle: '来自博客。',
    blogAll: '全部文章',
    promisesTitle: 'Volant 的承诺。',
    promises: [
      ['你的文件仍归你', '没有转换步骤，也没有新格式。同样的文件继续可用于 ansible-playbook。'],
      ['先拒绝', '这个版本无法运行的内容，大多会在第一次连接之前被点名，并且不会运行任何东西。'],
      ['优先 Linux 与 SSH', '目前支持通过 SSH 的 Linux 主机和本地连接。Windows 和网络设备以后再支持。'],
      ['无遥测', '除 playbook 要求的网络调用和运行时下载外，Volant 不发起任何网络请求。'],
    ],
    openTitle: '公开构建。',
    openText: 'Volant 采用 GPL-3.0-or-later，与 Ansible 相同。路线图、issue 和设计背后的决策都是公开的。',
    involved: '参与进来',
  },
  performance: {
    title: 'Ansible 性能：Volant 对比 ansible-core 2.19',
    description:
      '对比启用 pipelining 的 ansible-core 2.19.12 测得的两个真实工作负载、背后的配置，以及复现它们的命令。',
    heading: '在真实 playbook 上测量。',
    lede: '两个工作负载，各由仓库中的一个脚本运行，对比启用 pipelining 的 ansible-core 2.19.12。本页的每个数字都来自这些脚本，测量日期为 {date}。',
    faster: '在 {workload} 上更快',
    workloads: {
      roles: {
        name: '四个 Galaxy role',
        inSentence: '四个 Galaxy role',
        detail: '在两台已收敛的 Ubuntu 24.04 主机上运行 geerlingguy.security、nginx、git 和 pip，每台主机 56 个 task',
      },
      k3s: {
        name: 'k3s-ansible site.yml',
        inSentence: 'k3s-ansible site.yml',
        detail: '针对一个已安装的集群运行官方 k3s-ansible playbook，一个 server 和一个 agent',
      },
    },
    ratios:
      '倍数相差很大是有原因的。这些 role 在已收敛的主机上不改变任何东西，所以参照对象的时间几乎都花在每个模块周围的机制上。k3s playbook 每一轮都会重启服务并运行安装脚本，这部分工作在两个引擎下耗时相同。',
    splitTitle: 'Volant 的时间花在哪里。',
    splitText: '中位数那一轮，按步骤拆分。原生步骤由主机上的 agent 用 Rust 应答。',
    k3sServer: 'k3s-ansible，server',
    forTwoHosts: '两台主机 {time}',
    steps: {
      connect: '打开连接',
      files: 'file ×8、lineinfile ×8、stat ×5',
      systemd: 'systemd，3 个 task',
      setup: 'setup，收集 facts',
      apt: 'apt，8 个 task',
      wire: '线路上的帧和 blob',
      prepare: '在控制节点上准备 task',
      moduleSet: '构建 Python 模块集',
      wait: '等待另一台主机',
      restart: '重启 k3s，server 和 agent',
      serviceFacts: 'service_facts',
      aptCache: '带 update_cache 的 apt',
      packageFacts: 'package_facts',
      facts: '收集 facts',
    },
    kinds: { native: '原生', nativeSystemd: '原生 systemd', cached: '已缓存', python: 'Python' },
    k3sWork:
      '在 k3s 上，23.5 秒中的大部分是 playbook 自己要求的工作：重启 k3s 服务、运行它的安装脚本，以及 13 个 <code>command</code> task。这些是主机上的进程，两个引擎付出的代价相同。<a class="text-link" href="/docs/project/performance/#what-is-left">剩下的部分</a>解释其余内容。',
    configTitle: '配置。',
    configText: '两边都使用默认设置，只有 pipelining 例外，它会让参照对象更快。',
    config: [
      ['受管主机', '两台 Ubuntu 24.04 主机，各 4 核'],
      ['控制节点', 'Ubuntu 26.04，8 核'],
      ['Volant', 'release 构建，默认设置：启用原生模块，每台主机一条 SSH 连接'],
      ['参照对象', 'ansible-core 2.19.12，pipelining = True，forks 保持默认值 5'],
      ['轮次', '每个引擎先预热一轮，再各计时三轮，报告中位数'],
      ['校验', '两个引擎的每一轮都必须以相同的 PLAY RECAP 结束，否则脚本失败'],
    ],
    ownTitle: '测量你自己的运行。',
    ownProfile: '在你自己的 playbook 上，<code>--profile</code> 会按步骤打印上面那种拆分。',
    ownRecipes:
      '要复现这两个工作负载，脚本会以 release 模式构建 Volant，在你指定的主机上运行，并打印中位数和倍数。它们需要用 <code>uv tool install</code> 安装的 ansible-core 2.19.12，以及两台账号拥有免密 <code>sudo</code> 的主机。脚本会修改这些主机，所以请使用可以重置的机器。<a class="text-link" href="/docs/project/performance/#reproducing-the-figures">这些脚本</a>',
    ownRun: '# Your own run, split by step',
    ownBench: '# The two workloads of this page',
    caveat:
      '这些数字描述的是两台主机上的两个 playbook。你的结果取决于网络、task 的内容、原生模块覆盖了其中多少个，以及同时操作多少台主机。<a class="text-link" href="/docs/project/performance/">完整的测量</a>',
  },
  compatibility: {
    title: 'Ansible 兼容性：Volant 目前能运行什么',
    description:
      'Volant 目前能运行多少 ansible-core 2.19.12 和 ansible 包的内容，由测试套件逐名称测量。',
    heading: '与 ansible-core 2.19 逐名称兼容。',
    lede: 'Volant 加载 ansible-core 2.19 的全部语法并复现其行为，因此任何差异都是 bug。测试套件列出 ansible-core 附带的每个关键字、选项和插件，并检查本版本对它们的处理。',
    coreRuns: '的 ansible-core 可运行：{total} 个名称中 {runs} 个，另有 {partial} 个部分可用',
    collectionsRun:
      '的 collection 模块和插件可运行：{total} 个中 {runs} 个。其余 {later} 个大多要等 Windows 和网络设备支持。',
    also: '正在检查一个项目？<a class="text-link" href="/docs/start/compatibility/">我的 playbook 能运行吗？</a>列出了三条命令，<a class="text-link" href="#known-gaps-note">已知缺口</a>列出了哪些情况会无提示地失败。',
    runs: '可运行',
    partialLegend: '部分：缺少某个参数或值',
    laterLegend: '以后：Windows、其他连接方式、其他输出 callback',
    source: '数字取自测试套件生成的支持矩阵。',
    groups: {
      language: 'playbook 语言',
      modules: '模块与模板',
      hosts: '主机、运行与命令行',
      collections: 'ansible 包中的 collection',
    },
    partial: '{n} 个部分',
    later: '{n} 个以后',
    checkTitle: '检查你自己的项目。',
    checkText: '三条命令，前两条不会碰任何主机。',
    steps: {
      read: {
        title: '只读取，不运行',
        note: '列表输出和 --syntax-check 不会因不支持的内容而停止，所以对任何 playbook 都有效。',
      },
      list: {
        title: '对比 task 列表',
        note: '如果与 ansible-playbook --list-tasks 的输出一致，说明 Volant 对你的 role、import 和 tag 的编译方式相同。',
      },
      limit: {
        title: '在一台主机上运行',
        note: '这个版本无法运行的内容，大多会在连接之前就停止运行，点出被拒绝的内容，并以退出码 4 退出。',
      },
    },
    gaps: '有些缺口是无提示的：运行时可能忽略某项设置而不报错。在重要的运行之前，请先<a class="text-link" href="/docs/start/compatibility/#known-gaps">阅读已知缺口</a>。',
  },
  community: {
    description:
      '如何报告与 Ansible 的差异、认领 issue、提问或为 Volant 做贡献，以及后续版本会带来什么。',
    heading: '公开构建。',
    lede: '代码、路线图、issue 和设计背后的决策都是公开的。欢迎提交 bug 报告、修复、功能和文档。',
    ways: {
      failed: {
        title: '我的运行失败了',
        text: '查看退出码和消息，把失败缩小到某一个 task，并与 ansible-playbook 对比。这一页会带你一步步走一遍。',
        action: '运行失败时',
      },
      difference: {
        title: '报告差异',
        text: '同一个 playbook 在 Volant 和 ansible-playbook 下行为不同，就是 bug。模板会询问复现所需的信息。',
        action: '提交兼容性报告',
      },
      issue: {
        title: '认领 issue',
        text: '每个已规划的 issue 都写明预期行为、验收标准和代码中的入手位置。开始之前请先在 issue 下留言。',
        action: '适合新手的 issue',
      },
      ask: {
        title: '提问或提议',
        text: '问题、想法和设计讨论请发到 Discussions。超出一次修复的改动，请先开一个 issue，把方案谈妥。',
        action: '前往 Discussions',
      },
      docs: {
        title: '改进文档',
        text: '每一页都有编辑链接和报告问题的链接。文档与引擎在同一个仓库中。',
        action: '阅读文档',
      },
    },
    setupTitle: '四条命令完成设置。',
    setupText:
      '仓库固定了 Rust 工具链，所以 rustup 会选对版本。每个改动都通过 pull request 提交，并附上对应所改内容的测试。',
    setupLink: '阅读 CONTRIBUTING.md',
    setupHooks: 'git hook 和这些脚本用到的工具',
    setupCheck: '格式化、lint 和测试套件',
    nextTitle: '接下来。',
    nextText: '目前已规划的版本，按顺序排列。每个版本都是 GitHub 上的一个里程碑，附带其 issue。',
    nextLink: '路线图',
    license: '许可证',
    licenseText:
      'GPL-3.0-or-later，与 Ansible 相同。<a class="text-link" href="/docs/decisions/0001-gpl-3-0-or-later-license/">决策记录 0001</a> 解释了原因。',
    security: '安全',
    securityText:
      '请私下报告漏洞，不要在公开 issue 中提交。<a class="text-link" href="{repo}/blob/main/SECURITY.md">安全策略</a>说明了方法。',
    support: '支持',
    supportText:
      '去哪里提问，以及需要附上什么信息才能得到帮助。<a class="text-link" href="{repo}/blob/main/SUPPORT.md">SUPPORT.md</a>',
    decisions: '决策',
    decisionsText:
      '影响用户所见的各项选择，都连同理由一起记录在案。<a class="text-link" href="/docs/decisions/">决策记录</a>',
  },
  console: {
    hosts: '本次运行的主机',
    output: '本次运行的输出',
    source: '本次运行的 playbook 和 inventory',
    provenance:
      '用 {version} 在五台主机上通过本地连接录制，并在此按阅读速度回放。输出就是它当时打印的内容。',
    states: { waiting: '等待中', ok: 'ok', changed: 'changed', skipped: 'skipped', failed: 'failed' },
  },
  traces: {
    label: '{workload}：ansible-core 运行 {ansible} 秒，Volant 运行 {volant} 秒',
    hint: '将鼠标移到曲线上查看。',
    running: '运行中',
    done: '{seconds} 秒后完成',
    caption:
      '{date} 三轮计时的中位数，对比启用 pipelining = True 的 {reference}。当对应引擎的运行正在进行时，线条处于高位。',
  },
  docs: {
    report: '报告本页的问题',
    staleTitle: '此翻译可能已过时',
    staleBody: '本页翻译之后，英文页面有过更新。',
    staleLink: '阅读英文页面',
  },
};
