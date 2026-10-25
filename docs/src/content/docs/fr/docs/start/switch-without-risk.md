---
sourceHash: a9a50a7c4f07
title: Changer sans risque
description: Enregistrez vos vrais runs d'ansible-playbook, rejouez-les dans Volant, et changez quand les rejeux concordent.
---

Avant de passer un playbook à Volant, vous pouvez vérifier que Volant aurait fait exactement ce qu'a fait `ansible-playbook`. Enregistrez vos vrais runs, rejouez-les dans Volant, et laissez le registre vous dire quand le playbook est prêt.

Un rejeu ne touche jamais un hôte. L'enregistrement vient d'un vrai run d'`ansible-playbook`. Volant exécute le même playbook et répond à chaque tâche qui atteindrait un hôte avec le résultat enregistré. Rien ne change sur vos machines, et vos runs d'`ansible-playbook` continuent comme avant.

## Enregistrer vos runs

L'enregistrement vient d'un callback de la collection `volant.compat`. Installez-la là où ansible-core trouve ses collections :

```sh
ansible-galaxy collection install \
  "git+https://github.com/lorica-labs/volant.git#/collections/ansible_collections/volant/compat/,main"
```

Puis activez-le dans `ansible.cfg` :

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

Chaque run d'`ansible-playbook` écrit désormais un enregistrement. `{{ timestamp }}` devient l'heure de début du run, et un nom qui finit par `.gz` est compressé. `VOLANT_RECORD_PATH` règle le chemin depuis l'environnement. [Enregistrements de runs](/fr/docs/reference/recording/) décrit le format.

Un enregistrement contient les résultats des tâches tels qu'ansible-core les a vus : il peut donc contenir des valeurs de vault déchiffrées et les variables extra du run. Il est créé avec le mode `0600`. Gardez les enregistrements hors du contrôle de version et des artefacts publics, et donnez une rétention courte à un artefact de CI.

## Les rejouer

Lancez le rejeu dans votre dépôt, avec les playbooks et l'inventaire du run enregistré :

```sh
volant shadow replay shadow/run-20261025T093000Z.jsonl.gz
```

Le run rejoué s'affiche sur stderr. Le rapport, sur stdout, liste ce qui diffère, en cinq sortes de la plus grave à la moins grave : `inventory`, `flow`, `loop`, `args` et `judgment`. `--json` donne un objet par divergence. Le code de sortie est 0 sans divergence, 1 s'il y en a au moins une, et 4 quand l'enregistrement ne peut pas être rejoué, par exemple parce qu'un playbook a changé depuis. [volant shadow](/fr/docs/reference/cli/shadow/) donne les détails.

Le rapport dit aussi ce qu'il n'a pas comparé, et pourquoi. Une tâche `no_log` est comparée sur son seul statut. Une valeur qui change d'un run à l'autre, comme `lookup('pipe')`, `lookup('env')` ou `now()`, est inconnue pendant un rejeu : un argument construit avec elle est compté sous `unknown:` au lieu d'être signalé comme une divergence.

## Savoir quand un playbook est prêt

Chaque rejeu ajoute une ligne à `.volant/shadow.jsonl`, à côté de votre `ansible.cfg`. Versionnez-le ou gardez-le dans le cache de votre CI. `volant shadow status` le lit :

```sh
volant shadow status --require-ready
```

Un playbook est `ready` quand ses derniers rejeux sont propres, `shadow_runs` d'affilée (5 par défaut), et ont comparé au moins la part `shadow_coverage` de ses tâches (0.9 par défaut). Un rejeu `dirty` ou une nouvelle version de Volant relance la série. Un playbook modifié ne la relance pas. Les deux réglages se mettent dans la section `[volant]` de `ansible.cfg` :

```ini
[volant]
shadow_runs = 5
shadow_coverage = 0.9
```

`--require-ready` sort avec 1 tant que chaque playbook n'est pas prêt, ce qui en fait une barrière en CI.

## En CI

Ce workflow GitHub Actions enregistre un run à chaque push sur `main`, le rejoue dans un second job et garde le registre dans le cache. L'enregistrement voyage comme un artefact gardé un jour.

```yaml
name: shadow

on:
  push:
    branches: [main]
  workflow_dispatch:

permissions: {}

jobs:
  record:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
        with:
          persist-credentials: false
      - name: Install ansible-core and the recording callback
        run: |
          pipx install ansible-core==2.19.12
          ansible-galaxy collection install \
            "git+https://github.com/lorica-labs/volant.git#/collections/ansible_collections/volant/compat/,main"
      - name: Run the playbook as usual, recorded
        env:
          ANSIBLE_CALLBACKS_ENABLED: volant.compat.record
          VOLANT_RECORD_PATH: shadow/run.jsonl.gz
        run: ansible-playbook -i inventory.ini site.yml
      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7
        with:
          name: shadow-recording
          path: shadow/run.jsonl.gz
          retention-days: 1

  replay:
    needs: record
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
        with:
          persist-credentials: false
      - uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333 # v8
        with:
          name: shadow-recording
          path: shadow
      - name: Restore the ledger
        uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        with:
          path: .volant/shadow.jsonl
          key: shadow-ledger-${{ github.run_id }}
          restore-keys: shadow-ledger-
      - name: Install Volant
        run: |
          curl -fsSL https://volant.sh/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Replay the run through Volant
        run: volant shadow replay shadow/run.jsonl.gz
      - name: Keep the ledger, the replay diverged or not
        if: always()
        uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        with:
          path: .volant/shadow.jsonl
          key: shadow-ledger-${{ github.run_id }}
      - name: Gate on the verdict
        run: volant shadow status --require-ready
```

Le job de rejeu échoue quand le rejeu diverge, et la barrière échoue tant que le playbook n'est pas prêt. Le registre est sauvegardé dans tous les cas, donc la série continue au run suivant.

## Quand ce n'est pas prêt

- `diverged` : le dernier rejeu a trouvé une divergence. Le rapport nomme l'hôte, le `fichier:ligne` de la tâche, ce qu'a fait ansible-core et ce qu'a fait Volant. Une divergence que Volant ne devrait pas avoir est un bug. [Signalez-la](https://github.com/lorica-labs/volant/issues/new?template=compat.yml) avec le rapport, pas l'enregistrement, qui peut contenir des secrets.
- `not-ready`, série trop courte : continuez à enregistrer et à rejouer. Chaque rejeu propre en ajoute un.
- `not-ready`, couverture trop faible : le rapport liste les tâches jamais comparées. Elles sont dans des branches que vos runs enregistrés n'ont pas prises, et un run qui les prend les ajoute.
