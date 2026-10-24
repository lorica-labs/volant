// The English strings of the site and of the parts of the documentation the project draws itself.
// Every other language has a file next to this one with the same keys; a key it leaves out falls
// back to the English text. Strings may hold inline HTML (links, code), kept as is in translations.
export const en = {
  nav: {
    docs: 'Docs',
    performance: 'Performance',
    compatibility: 'Compatibility',
    community: 'Community',
    blog: 'Blog',
  },
  header: {
    home: 'Volant, home',
    install: 'Install',
    menu: 'Menu',
    releaseNotes: 'Release notes',
    github: 'Volant on GitHub',
    language: 'Language',
    skip: 'Skip to content',
  },
  footer: {
    about: 'An engine for Ansible playbooks, written in Rust. Open source under GPL-3.0-or-later.',
    use: 'Use',
    reference: 'Reference',
    project: 'Project',
    source: 'Source',
    installation: 'Installation',
    quickstart: 'Quickstart',
    willItRun: 'Will my playbook run?',
    whenItFails: 'When a run fails',
    leaves: 'What it leaves on your machines',
    uninstall: 'Uninstall',
    cli: 'Command line',
    configuration: 'Configuration',
    matrix: 'Support matrix',
    changelog: 'Changelog',
    roadmap: 'Roadmap',
    contributing: 'Contributing',
    security: 'Security policy',
    license: 'License',
    telemetry: 'Volant collects no telemetry.',
    name: '<i>Volant</i> is French for steering wheel.',
  },
  copy: { copy: 'Copy', copied: 'Copied', manual: 'Select and copy', label: 'Copy the command' },
  lookup: {
    label: 'Is it supported? Look up a module, collection, filter or option',
    button: 'Look up',
  },
  // The site pages. `{name}` is a value the page fills in; keep it, anywhere in the sentence.
  home: {
    title: 'Volant: your Ansible playbooks, run by a fast engine written in Rust',
    description:
      'Volant reads the playbooks, roles and inventories you already have and runs them against Linux hosts over SSH, much faster than ansible-playbook.',
    heading: 'Your Ansible playbooks, run by a fast engine written&nbsp;in&nbsp;Rust.',
    lede: 'Volant reads the playbooks, roles and inventories you already have and runs them over SSH. Nothing to convert.',
    platforms: 'Linux and macOS, x86_64 and arm64. Ansible is not needed. Windows is not supported yet.',
    quickstart: 'Quickstart',
    source: 'Source on GitHub',
    badge: 'pre-alpha',
    status:
      'It runs real playbooks, not every playbook yet. Try it on a host you can afford to touch, and see <a class="text-link" href="/docs/#what-volant-leaves-on-your-machines">what it leaves on your machines</a>.',
    runTitle: 'One run, five hosts.',
    runText:
      'A real run of Volant, replayed. Each spoke of the wheel stands for a host and takes the colour of the result it returns, in the colours ansible-playbook uses.',
    howTitle: 'How it works.',
    howText:
      'ansible-playbook pays for a new Python interpreter on the host for every task. Volant pays once per run, and keeps one connection open for the rest.',
    everyTask: 'for every task, on every host',
    slowSteps: ['build the module payload', 'send it over SSH', 'start Python', 'import module_utils', 'run the module'],
    oncePerRun: 'once per run',
    onceSteps: ['upload a small agent, cached on the host', 'open one SSH connection per host'],
    then: 'then',
    forEveryTask: 'for every task',
    fastSteps: ['send the task over the open link', 'answer it in Rust, or fork a warm Python'],
    howNote:
      'Common modules such as <code>apt</code>, <code>systemd</code>, <code>file</code>, <code>lineinfile</code> and fact gathering are answered in Rust by the agent when a task’s arguments allow it. Every other module runs in Python, in a server that loaded its imports once. <a class="text-link" href="/docs/internals/architecture/">How the engine is built</a>',
    filesTitle: 'Same files, same command.',
    filesText:
      '<code>volant playbook</code> takes the options of <code>ansible-playbook</code> and prints the output you know, down to the recap. Everything your project already has is read as it is.',
    filesLink: 'Run your first playbook',
    files: [
      ['ansible.cfg', 'read from the same places, with the same precedence'],
      ['Inventories', 'INI, YAML, TOML, scripts and inventory plugins'],
      ['group_vars and host_vars', 'layered in Ansible’s order'],
      ['Vault', 'vaulted files and values, --ask-vault-pass and --vault-id'],
      ['Roles and collections', 'dependencies, argument specs and installed collections'],
      ['Command line', 'the options of ansible-playbook, including --check and --diff'],
    ],
    refusalTitle: 'It says no before it touches a host.',
    refusalText:
      'Volant checks the whole playbook before it connects. Most of what this release cannot run is named there, with the play or task it is in, and the run stops with code 4. Nothing has changed on any host.',
    refusalCheck:
      '<code>--syntax-check</code> and the listings never stop on what is not supported, and <code>--check</code> with <code>--diff</code> shows what a real run would change. <a class="text-link" href="/docs/start/compatibility/">Check a project</a>',
    refusalCaption: 'Real output of Volant 0.1.0-alpha.10, for a play that asks for a strategy this release does not have.',
    runtime: [
      ['What it fetches', 'its own Python and ansible-core, about 155 MB, the first time a playbook needs Python'],
      ['How it checks it', 'every file against a hash pinned in the release'],
      ['Where it keeps it', '<code>~/.local/share/volant/runtime</code>, apart from any system Python'],
      ['Collections', '<code>volant runtime install --collections community.general</code>'],
      ['Without a network', '<code>volant runtime pack</code> on one machine, <code>install --from</code> on the other'],
    ],
    runtimeTitle: 'No Ansible to install.',
    runtimeText:
      'The binary carries the agents it uploads to your hosts. When a playbook needs a Python module, Volant installs a runtime of its own and keeps it apart from the rest of the machine.',
    runtimeLink: 'The managed runtime',
    faster: 'faster on {workload}, measured against ansible-core with pipelining',
    coreShare: 'of ansible-core’s {total} keywords, options and plugins run today',
    blogTitle: 'From the blog.',
    blogAll: 'Every post',
    promisesTitle: 'What Volant promises.',
    promises: [
      ['Your files stay yours', 'No conversion step and no new format. The same files keep working with ansible-playbook.'],
      ['Refusals come first', 'Most of what this release cannot run is named before the first connection, and nothing runs.'],
      ['Linux over SSH first', 'Linux hosts over SSH and local connections today. Windows and network devices come later.'],
      ['No telemetry', 'Volant makes no network call other than the ones your playbook asks for, and the runtime download.'],
    ],
    openTitle: 'Built in the open.',
    openText:
      'Volant is GPL-3.0-or-later, like Ansible. The roadmap, the issues and the decisions behind the design are public.',
    involved: 'Get involved',
  },
  performance: {
    description:
      'Two real workloads measured against ansible-core 2.19.12 with pipelining, the configuration behind them, and the commands that reproduce them.',
    heading: 'Measured on real playbooks.',
    lede: 'Two workloads, each run by a recipe in the repository, against ansible-core 2.19.12 with pipelining turned on. Every figure on this page comes from those recipes, measured on {date}.',
    faster: 'faster on {workload}',
    // The workloads of src/lib/bench.ts, by id. `inSentence` is the name as it reads after "faster on".
    workloads: {
      roles: {
        name: 'Four Galaxy roles',
        inSentence: 'four galaxy roles',
        detail: 'geerlingguy.security, nginx, git and pip on two converged Ubuntu 24.04 hosts, 56 tasks per host',
      },
      k3s: {
        name: 'k3s-ansible site.yml',
        inSentence: 'k3s-ansible site.yml',
        detail: 'the official k3s-ansible playbook against an installed cluster, one server and one agent',
      },
    },
    ratios:
      'The ratios are far apart for a reason. The roles change nothing on a converged host, so almost all of the reference’s time goes to the machinery around each module. The k3s playbook restarts services and runs install scripts on every pass, and that work takes as long under either engine.',
    splitTitle: 'Where Volant spends its time.',
    splitText: 'The median pass, split by step. A native step is answered in Rust by the agent on the host.',
    k3sServer: 'k3s-ansible, the server',
    forTwoHosts: '{time} for two hosts',
    // The steps of the split, by id, and the kind of each.
    steps: {
      connect: 'Opening the connections',
      files: 'file ×8, lineinfile ×8, stat ×5',
      systemd: 'systemd, 3 tasks',
      setup: 'setup, gathering facts',
      apt: 'apt, 8 tasks',
      wire: 'Frames and blobs on the wire',
      prepare: 'Preparing tasks on the controller',
      moduleSet: 'Building the Python module set',
      wait: 'Waiting for the other host',
      restart: 'Restarting k3s, server and agent',
      serviceFacts: 'service_facts',
      aptCache: 'apt with update_cache',
      packageFacts: 'package_facts',
      facts: 'Gathering facts',
    },
    kinds: { native: 'native', nativeSystemd: 'native systemd', cached: 'cached', python: 'Python' },
    k3sWork:
      'On k3s, most of the 23.5 seconds is work the playbook asks for: restarting the k3s service, running its install script and 13 <code>command</code> tasks. Both engines pay for those the same way, because they are processes on the host. <a class="text-link" href="/docs/project/performance/#what-is-left">What is left</a> explains the rest.',
    configTitle: 'The configuration.',
    configText: 'Default settings on both sides, except pipelining, which makes the reference faster.',
    config: [
      ['Managed hosts', 'two Ubuntu 24.04 hosts, 4 cores each'],
      ['Controller', 'Ubuntu 26.04, 8 cores'],
      ['Volant', 'release build, default settings: native modules on, one SSH connection per host'],
      ['Reference', 'ansible-core 2.19.12 with pipelining = True, forks at its default of 5'],
      ['Passes', 'one warm-up pass per engine, then three timed passes each; the median is reported'],
      ['Check', 'every pass must end on the same PLAY RECAP under both engines, or the recipe fails'],
    ],
    ownTitle: 'Measure your own run.',
    ownProfile: 'On your own playbook, <code>--profile</code> prints the split above for your run, step by step.',
    ownRecipes:
      'To reproduce the two workloads, the recipes build Volant in release mode, run against the hosts you name and print the medians and the ratio. They need ansible-core 2.19.12 installed with <code>uv tool install</code>, two hosts where the account has <code>sudo</code> without a password, and they change those hosts, so use machines you can wipe. <a class="text-link" href="/docs/project/performance/#reproducing-the-figures">The recipes</a>',
    ownRun: '# Your own run, split by step',
    ownBench: '# The two workloads of this page',
    caveat:
      'These figures describe two playbooks on two hosts. Yours depend on your network, on what your tasks do, on how many of them a native module covers and on how many hosts you drive at once. <a class="text-link" href="/docs/project/performance/">The full measurement</a>',
  },
  compatibility: {
    description:
      'How much of ansible-core 2.19.12 and of the ansible package Volant runs today, measured name by name by the test suite.',
    heading: 'Compatible with ansible-core 2.19, name by name.',
    lede: 'Volant loads the whole ansible-core 2.19 grammar and reproduces its behaviour, so any difference is a bug. The test suite lists every keyword, option and plugin ansible-core ships and checks what this release does with each.',
    coreRuns: 'of ansible-core runs: {runs} of {total} names, {partial} more in part',
    collectionsRun:
      'of the collections’ modules and plugins: {runs} of {total}. Most of the {later} others come with Windows and network device support.',
    also: 'Checking one project? <a class="text-link" href="/docs/start/compatibility/">Will my playbook run?</a> shows the three commands, and the <a class="text-link" href="#known-gaps-note">known gaps</a> list what fails without a message.',
    runs: 'runs',
    partialLegend: 'partial: an argument or a value is missing',
    laterLegend: 'later: Windows, other connections, other output callbacks',
    source: 'The numbers are read from the support matrix the test suite generates.',
    groups: {
      language: 'The playbook language',
      modules: 'Modules and templating',
      hosts: 'Hosts, runs and the command line',
      collections: 'Collections in the ansible package',
    },
    partial: '{n} partial',
    later: '{n} later',
    checkTitle: 'Check your own project.',
    checkText: 'Three commands, and the first two never touch a host.',
    steps: {
      read: {
        title: 'Read it without running it',
        note: 'Listings and --syntax-check never stop on something unsupported, so they work on any playbook.',
      },
      list: {
        title: 'Compare the task list',
        note: 'If it matches what ansible-playbook --list-tasks prints, Volant compiled your roles, imports and tags the same way.',
      },
      limit: {
        title: 'Run it on one host',
        note: 'Most of what this release cannot run stops the run before it connects, names what it refused, and exits with code 4.',
      },
    },
    gaps: 'A few gaps are silent: a run can ignore a setting there without an error. <a class="text-link" href="/docs/start/compatibility/#known-gaps">Read the known gaps</a> before a run that matters.',
  },
  community: {
    description:
      'How to report a difference from Ansible, take an issue, ask a question or contribute to Volant, and what the next releases bring.',
    heading: 'Built in the open.',
    lede: 'The code, the roadmap, the issues and the decisions behind the design are public. Bug reports, fixes, features and documentation are all welcome.',
    ways: {
      failed: {
        title: 'My run failed',
        text: 'Read the exit code and the message, narrow the failure down to one task, and compare with ansible-playbook. The page walks through it.',
        action: 'When a run fails',
      },
      difference: {
        title: 'Report a difference',
        text: 'A playbook that behaves differently under Volant and ansible-playbook is a bug. The template asks for what it takes to reproduce it.',
        action: 'Open a compatibility report',
      },
      issue: {
        title: 'Take an issue',
        text: 'Each planned issue gives the expected behaviour, the acceptance criteria and where to start in the code. Comment on it before you begin.',
        action: 'Good first issues',
      },
      ask: {
        title: 'Ask or propose',
        text: 'Questions, ideas and design discussions go to Discussions. For anything larger than a fix, open an issue first so the approach is agreed.',
        action: 'Go to Discussions',
      },
      docs: {
        title: 'Improve the documentation',
        text: 'Every page has a link to edit it and a link to report a problem with it. The documentation lives in the same repository as the engine.',
        action: 'Read the documentation',
      },
    },
    setupTitle: 'Set up in four commands.',
    setupText:
      'The repository pins its Rust toolchain, so rustup picks the right version. Every change goes through a pull request, with tests for what it touches.',
    setupLink: 'Read CONTRIBUTING.md',
    setupHooks: 'git hooks and the tools the recipes use',
    setupCheck: 'formatting, lints and the test suite',
    nextTitle: 'What comes next.',
    nextText: 'The releases planned so far, in order. Each one is a milestone on GitHub with its issues.',
    nextLink: 'The roadmap',
    license: 'License',
    licenseText:
      'GPL-3.0-or-later, like Ansible. <a class="text-link" href="/docs/decisions/0001-gpl-3-0-or-later-license/">Decision record 0001</a> explains why.',
    security: 'Security',
    securityText:
      'Report a vulnerability privately, never in a public issue. <a class="text-link" href="{repo}/blob/main/SECURITY.md">The security policy</a> says how.',
    support: 'Support',
    supportText:
      'Where to ask, and what to include so someone can help. <a class="text-link" href="{repo}/blob/main/SUPPORT.md">SUPPORT.md</a>',
    decisions: 'Decisions',
    decisionsText:
      'The choices that shape what users see are written down with their reasons. <a class="text-link" href="/docs/decisions/">The decision records</a>',
  },
  // The site components.
  console: {
    hosts: 'Hosts of the run',
    output: 'Output of the run',
    source: 'The playbook and inventory of this run',
    provenance:
      'Recorded with {version} on five hosts over a local connection, and replayed here at reading speed. The output is what it printed.',
    // A host's state in words, next to its colour.
    states: { waiting: 'waiting', ok: 'ok', changed: 'changed', skipped: 'skipped', failed: 'failed' },
  },
  traces: {
    label: '{workload}: ansible-core runs for {ansible} seconds, Volant for {volant} seconds',
    hint: 'Move over the trace to read it.',
    running: 'running',
    done: 'done after {seconds} s',
    caption:
      'Median of three timed passes on {date}, against {reference} with pipelining = True. A line is high while that engine’s run is going.',
  },
  docs: {
    report: 'Report a problem with this page',
    staleTitle: 'This translation may be out of date',
    staleBody: 'The English page changed after this page was translated.',
    staleLink: 'Read the English page',
  },
};

export type Strings = typeof en;

/** A translation: the same keys as English, any of them left out. */
export type Deep<T> = { [K in keyof T]?: T[K] extends object ? Deep<T[K]> : T[K] };
