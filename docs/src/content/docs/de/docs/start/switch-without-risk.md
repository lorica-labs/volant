---
sourceHash: a9a50a7c4f07
title: Ohne Risiko umsteigen
description: Echte Läufe von ansible-playbook aufzeichnen, in Volant wiederholen und umsteigen, sobald die Wiederholungen übereinstimmen.
---

Bevor Sie ein Playbook auf Volant umstellen, können Sie prüfen, dass Volant genau das getan hätte, was `ansible-playbook` getan hat. Zeichnen Sie Ihre echten Läufe auf, wiederholen Sie sie in Volant, und lassen Sie sich vom Register sagen, wann das Playbook bereit ist.

Eine Wiederholung berührt nie einen Host. Die Aufzeichnung stammt aus einem echten Lauf von `ansible-playbook`. Volant führt dasselbe Playbook aus und beantwortet jeden Task, der einen Host erreichen würde, mit dem aufgezeichneten Ergebnis. Auf Ihren Maschinen ändert sich nichts, und Ihre Läufe mit `ansible-playbook` gehen weiter wie bisher.

## Läufe aufzeichnen

Die Aufzeichnung stammt von einem Callback der Collection `volant.compat`. Installieren Sie sie dort, wo ansible-core seine Collections findet:

```sh
ansible-galaxy collection install \
  "git+https://github.com/lorica-labs/volant.git#/collections/ansible_collections/volant/compat/,main"
```

Aktivieren Sie ihn dann in `ansible.cfg`:

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

Jeder Lauf von `ansible-playbook` schreibt nun eine Aufzeichnung. `{{ timestamp }}` wird zur Startzeit des Laufs, und ein Name, der auf `.gz` endet, wird komprimiert. `VOLANT_RECORD_PATH` setzt den Pfad aus der Umgebung. [Aufzeichnungen von Läufen](/de/docs/reference/recording/) beschreibt das Format.

Eine Aufzeichnung enthält die Task-Ergebnisse so, wie ansible-core sie gesehen hat, also auch entschlüsselte Vault-Werte und die Extra-Variablen des Laufs. Sie wird mit Modus `0600` angelegt. Halten Sie Aufzeichnungen aus der Versionsverwaltung und aus öffentlichen Artefakten heraus, und geben Sie einem CI-Artefakt eine kurze Aufbewahrungsdauer.

## Wiederholen

Starten Sie die Wiederholung in Ihrem Repository, mit den Playbooks und dem Inventar des aufgezeichneten Laufs:

```sh
volant shadow replay shadow/run-20261025T093000Z.jsonl.gz
```

Der wiederholte Lauf erscheint auf stderr. Der Bericht auf stdout listet, was sich unterscheidet, in fünf Arten von der schwersten an: `inventory`, `flow`, `loop`, `args` und `judgment`. `--json` gibt ein Objekt pro Abweichung aus. Der Exit-Code ist 0 ohne Abweichung, 1 bei mindestens einer und 4, wenn sich die Aufzeichnung nicht wiederholen lässt, etwa weil sich ein Playbook seitdem geändert hat. [volant shadow](/de/docs/reference/cli/shadow/) beschreibt die Einzelheiten.

Der Bericht sagt auch, was er nicht verglichen hat und warum. Ein `no_log`-Task wird nur nach seinem Status verglichen. Ein Wert, der sich von Lauf zu Lauf ändert, etwa `lookup('pipe')`, `lookup('env')` oder `now()`, ist während einer Wiederholung unbekannt: Ein daraus gebautes Argument wird unter `unknown:` gezählt statt als Abweichung gemeldet.

## Wissen, wann ein Playbook bereit ist

Jede Wiederholung hängt eine Zeile an `.volant/shadow.jsonl` neben Ihrer `ansible.cfg` an. Versionieren Sie die Datei oder halten Sie sie im CI-Cache. `volant shadow status` liest sie:

```sh
volant shadow status --require-ready
```

Ein Playbook ist `ready`, sobald seine letzten Wiederholungen sauber waren, `shadow_runs` davon in Folge (standardmäßig 5), und mindestens den Anteil `shadow_coverage` seiner Tasks verglichen haben (standardmäßig 0.9). Eine `dirty`-Wiederholung oder eine neue Volant-Version startet die Serie neu. Ein geändertes Playbook nicht. Beide Einstellungen gehören in den Abschnitt `[volant]` der `ansible.cfg`:

```ini
[volant]
shadow_runs = 5
shadow_coverage = 0.9
```

`--require-ready` endet mit 1, bis jedes Playbook bereit ist, und taugt damit als Schranke in der CI.

## In der CI

Dieser GitHub-Actions-Workflow zeichnet bei jedem Push auf `main` einen Lauf auf, wiederholt ihn in einem zweiten Job und hält das Register im Cache. Die Aufzeichnung wandert als Artefakt, das einen Tag aufbewahrt wird.

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

Der Wiederholungsjob schlägt fehl, wenn die Wiederholung abweicht, und die Schranke schlägt fehl, bis das Playbook bereit ist. Das Register wird in jedem Fall gesichert, sodass die Serie im nächsten Lauf weiterläuft.

## Wenn es nicht bereit ist

- `diverged`: Die letzte Wiederholung hat eine Abweichung gefunden. Der Bericht nennt den Host, `Datei:Zeile` des Tasks, was ansible-core getan hat und was Volant getan hat. Eine Abweichung, die Volant nicht haben sollte, ist ein Fehler. [Melden Sie sie](https://github.com/lorica-labs/volant/issues/new?template=compat.yml) mit dem Bericht, nicht mit der Aufzeichnung, die Geheimnisse enthalten kann.
- `not-ready`, Serie zu kurz: weiter aufzeichnen und wiederholen. Jede saubere Wiederholung zählt eins dazu.
- `not-ready`, Abdeckung zu gering: Der Bericht listet die nie verglichenen Tasks. Sie liegen in Zweigen, die Ihre aufgezeichneten Läufe nicht genommen haben, und ein Lauf, der sie nimmt, fügt sie hinzu.
