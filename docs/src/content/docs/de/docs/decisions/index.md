---
sourceHash: 43d5378ddeb6
title: Entscheidungsdokumente
description: Die Entscheidungen, die prägen, was Volants Benutzer sehen, und warum sie getroffen wurden.
sidebar:
  label: Über Entscheidungsdokumente
  order: 0
---

Ein Entscheidungsdokument erklärt eine Wahl, die Volants Benutzer betrifft: wie die Lage war, was entschieden wurde und was daraus folgt. Die Dokumente verwenden das Format [MADR](https://adr.github.io/madr/) und sind in der Reihenfolge nummeriert, in der die Entscheidungen fielen.

Ein Dokument wird nicht umgeschrieben, sobald es akzeptiert ist. Ändert eine spätere Entscheidung es, sagt die Statuszeile das und verlinkt auf das neue Dokument. Messungen, die nach der Entscheidung gemacht wurden, kommen als datierte Abschnitte ans Ende.

| Dokument | Entscheidung |
|---|---|
| [0001](/de/docs/decisions/0001-gpl-3-0-or-later-license/) | Das Repository unter GPL-3.0-or-later lizenzieren. Schnittstellen-Crates sind die Ausnahme, siehe 0009. |
| [0002](/de/docs/decisions/0002-ansible-core-2-19-as-reference/) | ansible-core 2.19 nachbilden und jeden Unterschied als Bug behandeln. |
| [0003](/de/docs/decisions/0003-ansible-version-reports-the-reference-release/) | `ansible_version` meldet das Referenz-Release; `volant_version` meldet das von Volant. |
| [0004](/de/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) | `become` startet einen zweiten Agent unter `sudo`. |
| [0005](/de/docs/decisions/0005-fork-permits-span-host-local-batches/) | Ein Host behält seinen Fork-Permit über host-lokale Schritte hinweg, hinter der Option `batching`. |
| [0006](/de/docs/decisions/0006-warm-python-path/) | Pro Lauf einen Union-Blob mit Python-Modulen senden und sie von einem warmen Server forken. |
| [0007](/de/docs/decisions/0007-action-plugins/) | Action-Plugins als Folge von Teil-Tasks ausführen, die der Controller steuert. |
| [0008](/de/docs/decisions/0008-native-modules-and-speed/) | Gängige Module nativ im Agent beantworten, den Rest an Python übergeben und pro Host eine ssh-Verbindung teilen. |
| [0009](/de/docs/decisions/0009-interface-crates-mit-or-apache/) | Die Engine unter GPL-3.0-or-later halten und Crates, die eine öffentliche Schnittstelle beschreiben, unter MIT OR Apache-2.0 lizenzieren. |
| [0010](/de/docs/decisions/0010-core-version-profiles/) | Jedes Projekt wählt die ansible-core-Version, die Volant nachbildet (`2.19` oder `latest`), was Ansible entfernt hat, bleibt mit einer Warnung erhalten, und `volant upgrade-plan` listet auf, was ein Upgrade ändert. |
