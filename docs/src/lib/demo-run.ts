// A real run, recorded with `volant playbook -i inventory.ini site.yml` on Volant 0.1.0-alpha.10,
// five hosts over a local connection. The output below is what it printed, trailing spaces aside; the console
// on the home page replays it at reading speed. Record it again if the output format changes.
export const recordedWith = 'Volant 0.1.0-alpha.10';

export const hosts = ['web1', 'web2', 'web3', 'db1', 'db2'];

export const inventory = `[web]
web1
web2
web3

[db]
db1
db2

[all:vars]
ansible_connection=local
ansible_python_interpreter=/usr/bin/python3`;

export const playbook = `- hosts: all
  gather_facts: false
  tasks:
    - name: Check the host answers
      ping:

    - name: Write the app config
      copy:
        dest: "/tmp/volant-demo/out/{{ inventory_hostname }}.conf"
        content: "role={{ group_names | first }}\\n"

    - name: Reload the web server
      command: "true"
      when: "\\"web\\" in group_names"

    - name: Check the database port
      command: "true"
      changed_when: false
      when: "\\"db\\" in group_names"`;

export const output = `PLAY [all] *********************************************************************

TASK [Check the host answers] **************************************************
ok: [web1]
ok: [web2]
ok: [web3]
ok: [db1]
ok: [db2]

TASK [Write the app config] ****************************************************
changed: [web1]
changed: [web2]
changed: [web3]
changed: [db1]
changed: [db2]

TASK [Reload the web server] ***************************************************
changed: [web1]
changed: [web2]
changed: [web3]
skipping: [db1]
skipping: [db2]

TASK [Check the database port] *************************************************
skipping: [web1]
skipping: [web2]
skipping: [web3]
ok: [db1]
ok: [db2]

PLAY RECAP *********************************************************************
db1                        : ok=3    changed=1    unreachable=0    failed=0    skipped=1    rescued=0    ignored=0
db2                        : ok=3    changed=1    unreachable=0    failed=0    skipped=1    rescued=0    ignored=0
web1                       : ok=3    changed=2    unreachable=0    failed=0    skipped=1    rescued=0    ignored=0
web2                       : ok=3    changed=2    unreachable=0    failed=0    skipped=1    rescued=0    ignored=0
web3                       : ok=3    changed=2    unreachable=0    failed=0    skipped=1    rescued=0    ignored=0`;
