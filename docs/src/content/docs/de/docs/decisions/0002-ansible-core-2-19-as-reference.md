---
sourceHash: 833100c102c9
title: "ansible-core 2.19 als Kompatibilitätsreferenz verwenden"
description: "Entscheidungsdokument 0002."
sidebar:
  label: "0002 · Kompatibilitätsreferenz"
  order: 2
---

| Status | Datum |
|---|---|
| akzeptiert | 2026-09-05 |

## Kontext

Volant ist dafür gebaut, bestehende Playbooks unverändert auszuführen. „Unverändert“ braucht eine Definition: ansible-core ändert sein Verhalten zwischen Versionen, und 2.19 hat das Templating (Data Tagging) mit einigen beabsichtigten Brüchen neu geschrieben.

## Entscheidung

Volant bildet das Verhalten von ansible-core 2.19 nach. Unterschiede zu 2.19 sind Bugs. Golden-Tests und das Kompatibilitäts-Harness vergleichen mit dieser Version. Ein nächtlicher Job gegen den Entwicklungszweig von ansible-core, um Änderungen zu sehen, bevor sie in ein Release gelangen, ist geplant, aber nicht gebaut: Heute beobachtet nichts diesen Zweig.

## Konsequenzen

- Playbooks, die nur auf älteren Cores liefen, weil sie sich auf Verhalten stützten, das in 2.19 entfernt wurde, liegen außerhalb des Umfangs.
- Die Referenz auf einen neueren Core zu verschieben ist eine neue Entscheidung, hier festgehalten, mit einem Major- oder Minor-Versionssprung je nachdem, was sich ändert.
