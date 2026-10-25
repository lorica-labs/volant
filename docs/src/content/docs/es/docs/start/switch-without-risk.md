---
sourceHash: a9a50a7c4f07
title: Cambiar sin riesgo
description: Graba tus ejecuciones reales de ansible-playbook, reprodúcelas en Volant y cambia cuando las reproducciones coincidan.
---

Antes de pasar un playbook a Volant, puedes comprobar que Volant habría hecho exactamente lo que hizo `ansible-playbook`. Graba tus ejecuciones reales, reprodúcelas en Volant y deja que el registro te diga cuándo el playbook está listo.

Una reproducción nunca toca un host. La grabación viene de una ejecución real de `ansible-playbook`. Volant ejecuta el mismo playbook y responde a cada tarea que llegaría a un host con el resultado grabado. Nada cambia en tus máquinas, y tus ejecuciones de `ansible-playbook` siguen como antes.

## Grabar tus ejecuciones

La grabación viene de un callback de la colección `volant.compat`. Instálala donde ansible-core encuentra sus colecciones:

```sh
ansible-galaxy collection install \
  "git+https://github.com/lorica-labs/volant.git#/collections/ansible_collections/volant/compat/,main"
```

Después actívalo en `ansible.cfg`:

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

Cada ejecución de `ansible-playbook` escribe ahora una grabación. `{{ timestamp }}` se convierte en la hora de inicio de la ejecución, y un nombre que termina en `.gz` se comprime. `VOLANT_RECORD_PATH` define la ruta desde el entorno. [Grabaciones de ejecuciones](/es/docs/reference/recording/) describe el formato.

Una grabación guarda los resultados de las tareas tal como los vio ansible-core, así que puede contener valores de vault descifrados y las variables extra de la ejecución. Se crea con el modo `0600`. Mantén las grabaciones fuera del control de versiones y de los artefactos públicos, y da a un artefacto de CI una retención corta.

## Reproducirlas

Lanza la reproducción en tu repositorio, con los playbooks y el inventario de la ejecución grabada:

```sh
volant shadow replay shadow/run-20261025T093000Z.jsonl.gz
```

La ejecución reproducida se muestra en stderr. El informe en stdout lista lo que difiere, en cinco tipos del más grave al menos grave: `inventory`, `flow`, `loop`, `args` y `judgment`. `--json` da un objeto por divergencia. El código de salida es 0 sin divergencias, 1 con al menos una, y 4 cuando la grabación no se puede reproducir, por ejemplo porque un playbook cambió después. [volant shadow](/es/docs/reference/cli/shadow/) tiene los detalles.

El informe también dice qué no comparó y por qué. Una tarea `no_log` se compara solo por su estado. Un valor que cambia de una ejecución a otra, como `lookup('pipe')`, `lookup('env')` o `now()`, es desconocido durante una reproducción: un argumento construido con él se cuenta bajo `unknown:` en lugar de señalarse como divergencia.

## Saber cuándo un playbook está listo

Cada reproducción añade una línea a `.volant/shadow.jsonl`, junto a tu `ansible.cfg`. Versiónalo o guárdalo en la caché de tu CI. `volant shadow status` lo lee:

```sh
volant shadow status --require-ready
```

Un playbook está `ready` cuando sus últimas reproducciones fueron limpias, `shadow_runs` seguidas (5 por defecto), y compararon al menos la parte `shadow_coverage` de sus tareas (0.9 por defecto). Una reproducción `dirty` o una nueva versión de Volant reinicia la racha. Un playbook modificado no. Los dos ajustes van en la sección `[volant]` de `ansible.cfg`:

```ini
[volant]
shadow_runs = 5
shadow_coverage = 0.9
```

`--require-ready` sale con 1 hasta que cada playbook está listo, lo que lo convierte en una barrera en CI.

## En CI

Este workflow de GitHub Actions graba una ejecución en cada push a `main`, la reproduce en un segundo job y guarda el registro en la caché. La grabación viaja como un artefacto que se conserva un día.

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

El job de reproducción falla cuando la reproducción diverge, y la barrera falla hasta que el playbook está listo. El registro se guarda en cualquier caso, así que la racha continúa en la siguiente ejecución.

## Cuando no está listo

- `diverged`: la última reproducción encontró una divergencia. El informe nombra el host, el `archivo:línea` de la tarea, lo que hizo ansible-core y lo que hizo Volant. Una divergencia que Volant no debería tener es un error. [Repórtala](https://github.com/lorica-labs/volant/issues/new?template=compat.yml) con el informe, no con la grabación, que puede contener secretos.
- `not-ready`, racha demasiado corta: sigue grabando y reproduciendo. Cada reproducción limpia suma una.
- `not-ready`, cobertura demasiado baja: el informe lista las tareas nunca comparadas. Están en ramas que tus ejecuciones grabadas no tomaron, y una ejecución que las tome las añade.
