---
sourceHash: 0f7f2af12690
title: Roadmap
description: Woran Volant arbeitet, Release für Release, und wo Sie einsteigen können.
---

Volant ist in der Pre-Alpha-Phase. Das [Volant-Roadmap-Board](https://github.com/orgs/lorica-labs/projects/1) hält den Plan als GitHub-Issues fest, nach Release gruppiert. Jedes Issue sagt, was zu bauen ist, wie sich ansible-core 2.19.12 verhält, wie das Ergebnis zu prüfen ist und wo im Code man anfängt, sodass jeder eines übernehmen kann. Das [Changelog](https://github.com/lorica-labs/volant/blob/main/CHANGELOG.md) hält fest, was tatsächlich ausgeliefert wurde.

## Releases

| Meilenstein | Was er bringt |
|---|---|
| [v0.1.0-alpha.11](https://github.com/lorica-labs/volant/milestone/2) | Die Befehlssuite: `volant adhoc`, `volant vault` und `volant inventory` in Rust; `doc`, `config`, `galaxy`, `pull` und `console` über die verwaltete Runtime; optionale Befehlsnamen `ansible-*`, sodass vorhandene Skripte ohne installiertes ansible unverändert laufen |
| [v0.1.0-alpha.12](https://github.com/lorica-labs/volant/milestone/10) | Ein sicherer Umstieg: echte `ansible-playbook`-Läufe aufzeichnen, unter Volant abspielen, ohne die Hosts zu berühren, und die Engine-Version festschreiben |
| [v0.1.0-alpha.13](https://github.com/lorica-labs/volant/milestone/3) | Container-Verbindungen: docker, podman, kubectl, lxd und incus, chroot, buildah, aws_ssm |
| [v0.1.0-alpha.14](https://github.com/lorica-labs/volant/milestone/9) | Interne Arbeit: kürzere Executor-Funktionen, deklarative Tabellen, Speicherverbrauch |
| [v0.1.0-alpha.15](https://github.com/lorica-labs/volant/milestone/8) | Netzwerkgeräte (network_cli, netconf, httpapi), paramiko_ssh und die anderen stdout-Callbacks |
| [v0.1.0-alpha.16](https://github.com/lorica-labs/volant/milestone/4) | `volant galaxy` in Rust: Collections und Rollen |
| [v0.1.0-alpha.17](https://github.com/lorica-labs/volant/milestone/5) | `volant pull` und `volant console` in Rust |
| [v0.1.0-alpha.18](https://github.com/lorica-labs/volant/milestone/6) | Ein Windows-Controller: `volant.exe` |
| [v0.1.0-alpha.19](https://github.com/lorica-labs/volant/milestone/7) | Windows-Hosts: winrm, psrp, runas und die PowerShell-Module |

[Paritätslücken](https://github.com/lorica-labs/volant/milestone/12) listet die bisher gefundenen Unterschiede zu ansible-core auf. Sie sind an kein Release gebunden, und jede kann jederzeit behoben werden. [Paritätslücken, dritter Durchgang](https://github.com/lorica-labs/volant/milestone/14) sammelt die im Oktober 2026 gemessenen: ein Playbook, das unter Volant ein anderes Ergebnis, einen anderen Exit-Code oder eine andere Ausgabe liefert.

## Ein Issue übernehmen

Kommentieren Sie das Issue, bevor Sie anfangen, damit nicht zwei Personen dieselbe Arbeit machen. `size/S` ist etwa ein Tag, `size/M` einige Tage, `size/L` eine Woche oder mehr, und `good first issue` markiert einen sanften Einstieg. [CONTRIBUTING.md](https://github.com/lorica-labs/volant/blob/main/CONTRIBUTING.md) behandelt die Einrichtung und die Prüfungen.

Fehlt Ihnen etwas, das Sie blockiert? [Eröffnen Sie eine Feature-Anfrage](https://github.com/lorica-labs/volant/issues/new?template=feature.yml) oder starten Sie einen Thread in den [Discussions](https://github.com/lorica-labs/volant/discussions).
