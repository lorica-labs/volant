// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::BTreeSet;

use serde_json::Map;

use super::decide::native_gather_allowed;
use super::*;
use crate::compile::Compiled;
use crate::playbook::Play;
use crate::python::{Facts, Reach};
use crate::testing::scratch;
use crate::vars::VarStore;

/// A scratch tree for one test, removed when it goes out of scope.
struct Tree(tempfile::TempDir);

impl Tree {
    fn new() -> Tree {
        Tree(scratch())
    }

    fn file(&self, path: &str, text: &str) -> &Self {
        let path = self.0.path().join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        std::fs::write(path, text).expect("a file");
        self
    }

    /// Every play of `site.yml`, compiled with `roles/` beside it, read under `inventory`.
    fn read(&self, inventory: &str) -> FactsRead {
        let pb = crate::playbook::load(&self.0.path().join("site.yml"))
            .unwrap_or_else(|e| panic!("{e:#}"));
        let search = crate::roles::RoleSearch {
            paths: vec![self.0.path().join("roles")],
            collections: Vec::new(),
        };
        let compiled: Vec<Compiled> = pb
            .plays
            .iter()
            .map(|play| {
                crate::compile::compile(play, &search, &crate::compile::TagSelection::default())
                    .unwrap_or_else(|e| panic!("{e:#}"))
            })
            .collect();
        let walked: Vec<_> = pb
            .plays
            .iter()
            .map(|play| play.dir.as_path())
            .zip(&compiled)
            .collect();
        let reach = Reach::walk(&walked).unwrap_or_else(|e| panic!("{e:#}"));
        let inventory = crate::inventory::Inventory::parse_ini(inventory).expect("an inventory");
        let store =
            VarStore::new(&inventory, None, self.0.path(), Map::new()).expect("a var store");
        let plays: Vec<(&Play, &Compiled)> = pb.plays.iter().zip(&compiled).collect();
        facts_read(&plays, &reach, &store)
    }
}

fn keys(read: &FactsRead) -> &BTreeSet<String> {
    match read {
        FactsRead::Keys(keys) => keys,
        FactsRead::All(why) => panic!("every fact, for {why}"),
    }
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_string()).collect()
}

/// One task file holding `when` over `expr`, in a play of its own.
fn read_one(expr: &str) -> FactsRead {
    let tree = Tree::new();
    tree.file(
        "site.yml",
        &format!(
            "- hosts: all\n  tasks:\n    - name: t\n      debug:\n        msg: \"{{{{ {expr} }}}}\"\n"
        ),
    );
    tree.read("h1\n")
}

/// The four Galaxy roles of `just bench`, cut to every line that reads a fact, read only facts
/// the native `setup` gathers.
#[test]
fn the_proof_roles_read_only_native_facts() {
    let tree = Tree::new();
    tree.file(
        "site.yml",
        "- hosts: targets\n  become: true\n  roles:\n    - geerlingguy.security\n    - geerlingguy.nginx\n    - geerlingguy.git\n    - geerlingguy.pip\n",
    )
    .file(
        "roles/geerlingguy.security/tasks/main.yml",
        "- name: Include OS-specific variables.\n  include_vars: \"{{ ansible_facts.os_family }}.yml\"\n\
             - include_tasks: fail2ban.yml\n  when: security_fail2ban_enabled | bool\n\
             - include_tasks: autoupdate-Debian.yml\n  when:\n    - ansible_facts.os_family == 'Debian'\n    - security_autoupdate_enabled | bool\n",
    )
    .file(
        "roles/geerlingguy.security/tasks/fail2ban.yml",
        "- name: Install fail2ban (Debian).\n  package: name=fail2ban state=present\n  when: ansible_facts.os_family == 'Debian'\n\
             - name: Copy fail2ban custom configuration file into place.\n  template:\n    src: \"{{ security_fail2ban_custom_configuration_template }}\"\n    dest: /etc/fail2ban/jail.local\n\
             - name: Ensure fail2ban is running and enabled on boot.\n  service:\n    name: fail2ban\n    state: started\n",
    )
    .file(
        "roles/geerlingguy.security/tasks/autoupdate-Debian.yml",
        "- name: Copy unattended-upgrades configuration files in place.\n  template:\n    src: \"{{ item }}.j2\"\n    dest: \"/etc/apt/apt.conf.d/{{ item }}\"\n  with_items:\n    - 10periodic\n",
    )
    .file(
        "roles/geerlingguy.security/templates/jail.local.j2",
        "[sshd]\n{% if ansible_facts.os_family == 'Debian' and ansible_facts.distribution_major_version | int >= 12 %}\nbackend = systemd\n{% endif %}\n",
    )
    .file(
        "roles/geerlingguy.security/templates/10periodic.j2",
        "APT::Periodic::Update-Package-Lists \"1\";\nAPT::Periodic::Unattended-Upgrade \"1\";\n",
    )
    .file(
        "roles/geerlingguy.security/defaults/main.yml",
        "security_fail2ban_custom_configuration_template: \"jail.local.j2\"\n",
    )
    .file("roles/geerlingguy.security/vars/Debian.yml", "security_ssh_config_path: /etc/ssh/sshd_config\n")
    .file(
        "roles/geerlingguy.nginx/defaults/main.yml",
        "nginx_conf_template: \"nginx.conf.j2\"\nnginx_vhost_template: \"vhost.j2\"\nnginx_vhosts: []\nnginx_worker_processes: >-\n  \"{{ ansible_facts.processor_vcpus | default(ansible_facts.processor_count) }}\"\n",
    )
    .file(
        "roles/geerlingguy.nginx/tasks/main.yml",
        "- name: Include OS-specific variables.\n  include_vars: \"{{ ansible_facts.os_family }}.yml\"\n\
             - include_tasks: setup-Ubuntu.yml\n  when: ansible_facts.distribution == 'Ubuntu'\n\
             - name: Copy nginx configuration in place.\n  template:\n    src: \"{{ nginx_conf_template }}\"\n    dest: \"{{ nginx_conf_file_path }}\"\n\
             - import_tasks: vhosts.yml\n",
    )
    .file(
        "roles/geerlingguy.nginx/tasks/vhosts.yml",
        "- name: Add managed vhost config files.\n  template:\n    src: \"{{ item.template|default(nginx_vhost_template) }}\"\n    dest: \"{{ nginx_vhost_path }}/{{ item.filename }}\"\n  when: item.state|default('present') != 'absent'\n  with_items: \"{{ nginx_vhosts }}\"\n",
    )
    .file(
        "roles/geerlingguy.nginx/tasks/setup-Ubuntu.yml",
        "- name: Ensure nginx is installed.\n  apt:\n    name: \"{{ nginx_package_name }}\"\n    state: present\n",
    )
    .file(
        "roles/geerlingguy.nginx/templates/nginx.conf.j2",
        "user  {{ nginx_user }};\nworker_processes  {{ nginx_worker_processes }};\n# {{ ansible_managed }}\n",
    )
    .file(
        "roles/geerlingguy.git/tasks/main.yml",
        "- name: Ensure git is installed (Debian).\n  apt:\n    name: \"{{ git_packages }}\"\n    state: present\n  when: ansible_os_family == 'Debian'\n\
             - name: Include OS-specific variables (Fedora).\n  include_vars: \"{{ ansible_distribution }}.yml\"\n  when: ansible_distribution == \"Fedora\"\n",
    )
    .file(
        "roles/geerlingguy.pip/tasks/main.yml",
        "- name: Remove the EXTERNALLY-MANAGED file.\n  file:\n    path: \"/usr/lib/python3.{{ ansible_facts['python']['version']['minor'] }}/EXTERNALLY-MANAGED\"\n    state: absent\n\
             - name: Ensure pip is installed.\n  package:\n    name: \"{{ pip_package }}\"\n    state: present\n",
    );
    let read = tree.read(
        "[targets]\ntarget ansible_host=target ansible_python_interpreter=/usr/bin/python3\n",
    );
    assert_eq!(
        keys(&read),
        &set(&[
            "distribution",
            "distribution_major_version",
            "os_family",
            "pkg_mgr",
            "processor_count",
            "processor_vcpus",
            "python",
            "service_mgr",
        ])
    );
    assert!(native_gather_allowed(&read));
    assert_eq!(
        decide(&read),
        (Facts::Native, "native (keys: 8)".to_string())
    );
}

/// The k3s-ansible playbook of `just bench-k3s`, cut to every line that reads a fact, reads
/// only facts the native `setup` gathers.
#[test]
fn the_k3s_playbook_reads_only_native_facts() {
    let tree = Tree::new();
    tree.file(
        "site.yml",
        "- name: Cluster prep\n  hosts: k3s_cluster\n  gather_facts: true\n  roles:\n    - role: prereq\n    - role: raspberrypi\n\
             - name: Setup K3S server\n  hosts: server\n  gather_facts: false\n  roles:\n    - role: k3s_server\n",
    )
    .file(
        "roles/raspberrypi/tasks/main.yml",
        "- name: Run Raspberry Pi-specific tasks\n  when:\n    - raspberrypi_grep_cpuinfo.rc == 0\n  block:\n\
             \x20   - name: Set detected_distribution to Debian\n      ansible.builtin.set_fact:\n        detected_distribution: Debian\n      when: >\n        ansible_facts.lsb.id|default(\"\") == \"Debian\"\n\
             \x20   - name: Execute OS related tasks on the Raspberry Pi\n      ansible.builtin.include_tasks: \"{{ item }}\"\n      with_first_found:\n        - \"prereq/{{ detected_distribution }}.yml\"\n        - \"prereq/{{ ansible_facts['distribution'] }}-{{ ansible_facts['distribution_major_version'] }}.yml\"\n        - \"prereq/{{ ansible_facts['distribution'] }}.yml\"\n        - \"prereq/default.yml\"\n",
    )
    .file(
        "roles/raspberrypi/tasks/prereq/Debian.yml",
        "- name: Check if /boot/firmware/cmdline.txt exists\n  ansible.builtin.stat:\n    path: /boot/firmware/cmdline.txt\n  register: raspberrypi_boot_file\n",
    )
    .file("roles/raspberrypi/tasks/prereq/default.yml", "---\n")
    .file(
        "roles/prereq/tasks/main.yml",
        "- name: Set same timezone on every Server\n  community.general.timezone:\n    name: \"{{ system_timezone }}\"\n  when: (system_timezone is defined) and (system_timezone != \"Your/Timezone\")\n\
             - name: Populate service facts\n  ansible.builtin.service_facts:\n\
             - name: Gather package facts\n  ansible.builtin.package_facts:\n\
             - name: Allow UFW Exceptions\n  when:\n    - ansible_facts.services['ufw'] is defined\n    - ansible_facts.services['ufw'].state == 'running'\n  debug:\n    msg: ufw\n\
             - name: Add kernel module\n  when: \"'iptables' in ansible_facts.packages\"\n  debug:\n    msg: nft\n\
             - name: Allow nodes\n  when: ansible_facts.services['firewalld.service'] is defined\n  ansible.posix.firewalld:\n    source: \"{{ item }}\"\n    zone: internal\n  loop: >-\n    {{\n      (\n        groups[server_group] | default([])\n        + groups[agent_group] | default([])\n      )\n      | map('extract', hostvars)\n      | selectattr('ansible_facts.default_ipv4', 'defined')\n      | map(attribute='ansible_facts.default_ipv4.address')\n      | flatten | unique | list\n    }}\n\
             - name: Add nftables rules\n  ansible.builtin.template:\n    src: k3s.nft.j2\n    dest: /etc/nftables.d/k3s.nft\n\
             - name: Enable IPv6 forwarding\n  ansible.posix.sysctl:\n    name: net.ipv6.conf.all.forwarding\n    value: \"1\"\n  when: ansible_facts['all_ipv6_addresses']\n\
             - name: Add br_netfilter\n  when: ansible_facts['os_family'] == \"RedHat\" or ansible_facts['distribution'] == \"Archlinux\"\n  debug:\n    msg: x\n\
             - name: Check lsb\n  when: ansible_facts.lsb.id == 'Ubuntu' and ansible_facts['distribution_version'] is version('22.04', '>=')\n  debug:\n    msg: x\n\
             - name: Download k3s binary\n  ansible.builtin.get_url:\n    url: https://github.com/k3s-io/k3s/releases/download/{{ k3s_version }}/k3s{{ k3s_arch_suffix[ansible_facts['architecture']] }}\n    dest: /usr/local/bin/k3s\n  when: ansible_facts['service_mgr'] == 'systemd' and ansible_facts['kernel'] is defined\n\
             - name: Rename host\n  when: ansible_facts['hostname'] != inventory_hostname and ansible_check_mode is false\n  debug:\n    msg: \"{{ ansible_user }}@{{ ansible_host }}:{{ ansible_port | default(22) }} {{ ansible_connection }} {{ ansible_become }} {{ ansible_version.full }} {{ ansible_run_tags }}\"\n",
    )
    .file(
        "roles/prereq/templates/k3s.nft.j2",
        "{% for host in (groups[server_group] | default([]) + groups[agent_group] | default([])) | unique %}\n\
             {% if hostvars[host].ansible_default_ipv4 is defined %}\n\
             insert rule inet filter input ip saddr {{ hostvars[host].ansible_default_ipv4.address }} accept\n\
             {% endif %}\n{% endfor %}\n",
    )
    .file(
        "roles/k3s_server/tasks/main.yml",
        "- name: Add token\n  when: token is not defined and hostvars[groups[server_group][0]].random_token is defined\n  ansible.builtin.set_fact:\n    k3s_server_config: \"{{ k3s_server_config | combine({'token': hostvars[groups[server_group][0]].random_token}) }}\"\n",
    );
    let read = tree.read(
        "[server]\ntarget ansible_host=target\n[agent]\ngen ansible_host=gen\n[k3s_cluster:children]\nserver\nagent\n\
             [k3s_cluster:vars]\nansible_user=user\napi_endpoint=\"{{ hostvars[groups['server'][0]]['ansible_host'] | default(groups['server'][0]) }}\"\n",
    );
    assert_eq!(
        keys(&read),
        &set(&[
            "all_ipv6_addresses",
            "architecture",
            "default_ipv4",
            "distribution",
            "distribution_major_version",
            "distribution_version",
            "hostname",
            "kernel",
            "lsb",
            "os_family",
            "service_mgr",
        ])
    );
    assert!(native_gather_allowed(&read));
}

/// Review Focus 4: a fact read by a name only known once it renders reads any fact, and the
/// Python `setup` stays.
#[test]
fn a_dynamic_fact_read_keeps_python_facts() {
    for (name, expr) in [
        ("subscript", "ansible_facts[item]"),
        ("to-json", "hostvars[h] | to_json"),
        ("hostvars-alone", "hostvars | dict2items"),
        ("vars", "vars['ansible_' ~ x]"),
        ("vars-name", "vars[fact_name]"),
        ("lookup", "lookup('vars', 'ansible_' ~ x)"),
        ("query", "query('ansible.builtin.vars', 'y')"),
        ("template", "lookup('template', 'x.j2')"),
        ("whole", "ansible_facts | to_json"),
        ("get", "ansible_facts.get('mounts')"),
        (
            "extract-expr",
            "groups.all | map('extract', hostvars, item)",
        ),
        (
            "extract-whole",
            "groups.all | map('extract', hostvars) | map('to_json')",
        ),
        (
            "extract-facts",
            "groups.all | map('extract', hostvars, 'ansible_facts')",
        ),
        ("host-subscript", "hostvars[h][name]"),
        ("items", "hostvars[h].items()"),
        (
            "extract-attr-var",
            "groups.all | map('extract', hostvars) | map(attribute=addr_attr) | list",
        ),
        (
            "extract-select-var",
            "groups.all | map('extract', hostvars) | selectattr(fact_var, 'defined') | map(attribute='x')",
        ),
        (
            "legacy-template",
            "lookup('ansible.legacy.template', 'report.j2')",
        ),
        ("legacy-vars", "query('ansible.legacy.vars', fact_name)"),
        ("lookup-space", "lookup ('vars', fact_name)"),
    ] {
        let read = read_one(expr);
        assert!(
            matches!(read, FactsRead::All(_)),
            "{name}: {expr}: {read:?}"
        );
        assert!(!native_gather_allowed(&read), "{name}: {expr}");
        assert_eq!(decide(&read).0, Facts::Python, "{name}: {expr}");
    }
}

/// The literal forms each read one key: attribute, subscript, another host's by name and by
/// `extract` with a key, in its list form too.
#[test]
fn a_literal_fact_read_is_one_key() {
    for (name, expr, key) in [
        ("attr", "ansible_facts.mounts[0].size", "mounts"),
        ("subscript", "ansible_facts['devices']", "devices"),
        ("bare", "ansible_kernel_version", "kernel_version"),
        ("sub-key", "ansible_default_ipv4.address", "default_ipv4"),
        (
            "other-host",
            "hostvars['h2']['ansible_facts']['fqdn']",
            "fqdn",
        ),
        (
            "other-host-attr",
            "hostvars[h].ansible_facts.domain",
            "domain",
        ),
        (
            "extract",
            "groups.all | map('extract', hostvars, 'ansible_machine')",
            "machine",
        ),
        (
            "extract-list",
            "groups.all | map('extract', hostvars, ['ansible_facts', 'nodename'])",
            "nodename",
        ),
    ] {
        assert_eq!(keys(&read_one(expr)), &set(&[key]), "{name}: {expr}");
    }
}

/// A key the native does not collect keeps the Python `setup`, and the profile names it.
#[test]
fn a_key_outside_the_native_keeps_python_facts() {
    let read = read_one("ansible_mounts | map(attribute='mount')");
    assert_eq!(keys(&read), &set(&["mounts"]));
    assert!(!native_gather_allowed(&read));
    assert_eq!(
        decide(&read),
        (
            Facts::Python,
            "python (mounts read, not collected natively)".to_string()
        )
    );
}

/// Where the text is: a template with a literal `src` outside any role, a `vars_files` entry, a
/// play variable, a `group_vars` file, and an `include_vars` file.
#[test]
fn every_source_of_text_is_read() {
    let tree = Tree::new();
    tree.file(
        "site.yml",
        concat!(
            "- hosts: all\n",
            "  vars:\n",
            "    a: \"{{ ansible_fqdn }}\"\n",
            "  vars_files:\n",
            "    - vars/extra.yml\n",
            "  tasks:\n",
            "    - template:\n",
            "        src: conf.j2\n",
            "        dest: /etc/conf\n",
            "    - include_vars: more.yml\n",
        ),
    )
    .file("templates/conf.j2", "{{ ansible_mounts }}\n")
    .file("vars/extra.yml", "b: \"{{ ansible_devices }}\"\n")
    .file("vars/more.yml", "c: \"{{ ansible_lvm }}\"\n")
    .file(
        "group_vars/all.yml",
        "d: \"{{ ansible_uptime_seconds }}\"\n",
    );
    let read = tree.read("h1\n");
    assert_eq!(
        keys(&read),
        &set(&["devices", "fqdn", "lvm", "mounts", "uptime_seconds"])
    );
}

/// What the scan cannot open reads every fact: a templated name outside a role, a template that
/// is not there, a template pulling another in, and a role named by a template.
#[test]
fn what_the_scan_cannot_open_reads_every_fact() {
    for (name, task) in [
        (
            "src",
            "template:\n        src: \"{{ x }}.j2\"\n        dest: /x",
        ),
        (
            "missing",
            "template:\n        src: missing.j2\n        dest: /x",
        ),
        ("pull", "template:\n        src: pulls.j2\n        dest: /x"),
        ("include", "include_tasks: \"{{ x }}.yml\""),
        ("role", "include_role:\n        name: \"{{ x }}\""),
        ("vars", "include_vars: \"{{ x }}.yml\""),
        (
            "path-fact",
            "include_vars: \"{{ ansible_user_dir }}/vars.yml\"",
        ),
    ] {
        let tree = Tree::new();
        tree.file(
            "site.yml",
            &format!("- hosts: all\n  tasks:\n    - {task}\n"),
        )
        .file("templates/pulls.j2", "{% include 'other.j2' %}\n");
        let read = tree.read("h1\n");
        assert!(matches!(read, FactsRead::All(_)), "{name}: {read:?}");
    }
}

/// Puppet's and Chef's facts, which the Python `setup` adds without the `ansible_` prefix, are
/// keys the native never collects.
#[test]
fn facter_and_ohai_facts_keep_python_facts() {
    for (name, expr, key) in [
        ("facter", "facter_virtual == 'kvm'", "facter_virtual"),
        ("ohai", "ohai_platform", "ohai_platform"),
    ] {
        let read = read_one(expr);
        assert_eq!(keys(&read), &set(&[key]), "{name}: {expr}");
        assert!(!native_gather_allowed(&read), "{name}: {expr}");
    }
}

/// A templated file name that renders outside the role is followed: to `playbook_dir`, to the
/// playbook's `templates/`, and to an unread task file.
#[test]
fn a_templated_name_leaving_the_role_is_followed() {
    let role = |tree: &Tree| {
        tree.file("site.yml", "- hosts: all\n  roles:\n    - web\n")
            .file(
                "roles/web/defaults/main.yml",
                "conf: nginx.conf.j2\nhook: none.yml\n",
            )
            .file(
                "roles/web/templates/nginx.conf.j2",
                "{{ ansible_hostname }}\n",
            )
            .file("roles/web/tasks/none.yml", "- debug: msg=x\n")
            .file(
                "roles/web/tasks/main.yml",
                "- template:\n    src: \"{{ conf }}\"\n    dest: /etc/nginx.conf\n\
                     - include_tasks: \"{{ hook }}\"\n",
            );
    };
    let tree = Tree::new();
    role(&tree);
    tree.file(
        "group_vars/all.yml",
        "conf: \"{{ playbook_dir }}/templates/nginx.conf.j2\"\n",
    );
    assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

    let tree = Tree::new();
    role(&tree);
    tree.file("group_vars/all.yml", "conf: custom.j2\n").file(
        "templates/custom.j2",
        "{% if ansible_virtualization_type == 'lxc' %}x{% endif %}\n",
    );
    let read = tree.read("h1\n");
    assert_eq!(keys(&read), &set(&["hostname", "virtualization_type"]));
    assert!(!native_gather_allowed(&read));

    let tree = Tree::new();
    role(&tree);
    let hook = tree.0.path().join("hooks/extra.yml");
    tree.file("hooks/extra.yml", "- debug: msg=\"{{ ansible_mounts }}\"\n")
        .file("group_vars/all.yml", &format!("hook: {}\n", hook.display()));
    assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

    let tree = Tree::new();
    role(&tree);
    tree.file("hooks/extra.yml", "- debug: msg=\"{{ ansible_mounts }}\"\n")
        .file(
            "group_vars/all.yml",
            "hook: \"{{ playbook_dir }}/hooks/extra.yml\"\n",
        );
    assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

    // A variable named by a template can be any variable.
    let tree = Tree::new();
    role(&tree);
    tree.file(
        "roles/web/tasks/pick.yml",
        "- set_fact:\n    \"{{ which }}\": \"{{ playbook_dir }}/t.j2\"\n",
    );
    assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

    // `with_first_found` finds a file in the playbook's `files/` before the role's own.
    let tree = Tree::new();
    role(&tree);
    tree.file(
        "roles/web/tasks/os.yml",
        "- include_tasks: \"{{ item }}\"\n  with_first_found:\n    - \"prereq/{{ ansible_facts['distribution'] }}.yml\"\n    - prereq/default.yml\n",
    )
    .file("roles/web/tasks/prereq/default.yml", "- debug: msg=x\n")
    .file("files/prereq/Ubuntu.yml", "- debug: msg=\"{{ ansible_mounts }}\"\n");
    assert!(matches!(tree.read("h1\n"), FactsRead::All(_)));

    // Known values alone: a name read, a fact standing for any name, followed out of the
    // role to the playbook's `vars/`.
    let tree = Tree::new();
    role(&tree);
    tree.file("vars/Debian.yml", "x: \"{{ ansible_lsb }}\"\n")
        .file(
            "roles/web/tasks/more.yml",
            "- include_vars: \"{{ ansible_facts.os_family }}.yml\"\n",
        );
    assert_eq!(
        keys(&tree.read("h1\n")),
        &set(&["hostname", "lsb", "os_family"])
    );
}

/// A block's own keywords in a file only the walk reaches (a dynamic include here) are read.
#[test]
fn a_block_keyword_in_an_included_file_is_read() {
    let tree = Tree::new();
    tree.file("site.yml", "- hosts: all\n  roles:\n    - base\n")
        .file(
            "roles/base/tasks/main.yml",
            "- include_tasks: \"setup-{{ ansible_os_family }}.yml\"\n",
        )
        .file(
            "roles/base/tasks/setup-Debian.yml",
            "- block:\n    - debug: msg=x\n  when: ansible_virtualization_type != 'docker'\n  vars:\n    disk: \"{{ ansible_devices }}\"\n",
        );
    let read = tree.read("h1\n");
    assert_eq!(
        keys(&read),
        &set(&["devices", "os_family", "virtualization_type"])
    );
}

/// A play handler's include is followed, and what the included file reads counts.
#[test]
fn a_play_handler_include_is_read() {
    let tree = Tree::new();
    tree.file(
        "site.yml",
        "- hosts: all\n  tasks:\n    - debug: msg=x\n  handlers:\n    - name: reconfigure\n      include_tasks: handlers/reconfigure.yml\n",
    )
    .file(
        "handlers/reconfigure.yml",
        "- template:\n    src: fstab.j2\n    dest: /etc/fstab\n",
    )
    .file("templates/fstab.j2", "{% for m in ansible_mounts %}{{ m }}{% endfor %}\n");
    let read = tree.read("h1\n");
    assert_eq!(keys(&read), &set(&["mounts"]));
}
