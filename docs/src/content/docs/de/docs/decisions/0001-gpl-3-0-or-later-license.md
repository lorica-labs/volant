---
sourceHash: 8cfc8ab189fd
title: "GPL-3.0-or-later für das ganze Repository verwenden"
description: "Entscheidungsdokument 0001."
sidebar:
  label: "0001 · Lizenz"
  order: 1
---

| Status | Datum |
|---|---|
| akzeptiert; der Geltungsbereich wird durch [ADR 0009](/de/docs/decisions/0009-interface-crates-mit-or-apache/) eingeengt, das Crates, die eine öffentliche Schnittstelle beschreiben, unter MIT OR Apache-2.0 lizenziert | 2026-09-05 |

## Kontext

Volant ist dafür gebaut, unveränderte Ansible-Module auszuführen. Dazu ruft der Controller den eigenen Payload-Builder von ansible-core über einen eingebetteten Python-Interpreter auf, und der Agent liefert die `module_utils` von ansible-core an verwaltete Hosts. ansible-core steht unter GPL-3.0. Eine GPL-Bibliothek zur Laufzeit zu importieren und Teile davon weiterzugeben, macht eine permissive Lizenz für Volant rechtlich brüchig.

Dieses Release ist noch nicht so weit. Es bettet keinen Python-Interpreter ein, trägt keinen Code von ansible-core und führt nur die nativen Module aus. Die Lizenz ist für die Architektur gewählt, auf die das Projekt zusteuert, nicht für die, die es hat, denn ein Repository, dessen Mitwirkende alle unter einer Lizenz zugestimmt haben, lässt sich schwer auf eine andere umstellen.

## Entscheidung

Das ganze Repository steht unter GPL-3.0-or-later, der Lizenz von Ansible und der meisten Collections.

## Konsequenzen

- Mitwirkende bestätigen ihre Commits unter dem Developer Certificate of Origin.
- Die Verbreitung wird nicht berührt: Ansible selbst ist ein GPL-Kommandozeilenwerkzeug.
- Wenn der Payload-Builder in Rust neu geschrieben ist und der Agent keinen Code von ansible-core mehr enthält, darf das Agent-Crate unter Apache-2.0 neu lizenziert werden. Das wäre eine neue Entscheidung.

## Update, 2026-09-22

Der Kontext oben beschreibt das Projekt, als diese Entscheidung fiel. Seit [ADR 0006](/de/docs/decisions/0006-warm-python-path/) liefert Volant die `module_utils` von ansible-core an verwaltete Hosts, und der Controller ruft ansible-core über einen Hilfsprozess statt über einen eingebetteten Interpreter auf. Die Begründung für die Lizenz ist unverändert.

## Update, 2026-10-24

[Entscheidung 0009](/de/docs/decisions/0009-interface-crates-mit-or-apache/) hält die Engine unter GPL-3.0-or-later und lizenziert die Crates, die nur eine öffentliche Schnittstelle beschreiben, unter MIT OR Apache-2.0. Sie hält außerdem `volant-agent` und `volant-protocol` unter GPL-3.0-or-later, sodass die in den Konsequenzen oben genannte mögliche Neulizenzierung des Agent-Crates nicht geplant ist.
