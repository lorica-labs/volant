---
sourceHash: dad3d3f2ec02
title: "Die Engine unter GPL halten und Schnittstellen-Crates unter MIT OR Apache-2.0 lizenzieren"
description: "Entscheidungsdokument 0009."
sidebar:
  label: "0009 · Schnittstellen-Lizenzen"
  order: 9
---

| Status | Datum |
|---|---|
| akzeptiert | 2026-10-24 |

## Kontext

Andere Programme wollen Volant steuern und lesen, was ein Lauf getan hat: CI-Systeme, Editoren, Dashboards und KI-Agents. Heute können sie nur die menschenlesbare Ausgabe parsen, die dem Textformat von Ansible folgt und für Menschen geschrieben ist. Geplant ist ein dokumentierter Event-Stream mit einem kleinen Crate, `volant-events`, das seine Typen und sein JSON Schema enthält, damit ein Rust-Programm sie nicht noch einmal definieren muss.

[Entscheidung 0001](/de/docs/decisions/0001-gpl-3-0-or-later-license/) stellte das ganze Repository unter GPL-3.0-or-later. Unter dieser Lizenz müsste jedes Programm, das `volant-events` linkt, ebenfalls GPL sein, was die meisten Werkzeuge davon abhalten würde, eine Schnittstelle zu nutzen, die für alle gedacht ist.

## Entscheidung

Die Engine bleibt unter GPL-3.0-or-later. Das umfasst `volant`, `volant-agent`, `volant-protocol` und alles andere im Repository außer den Crates unten.

Ein Crate, dessen einziger Zweck es ist, eine öffentliche Schnittstelle für andere Programme zu beschreiben, wird unter MIT OR Apache-2.0 lizenziert. Das erste wird `volant-events` sein.

Andere Programme sprechen mit Volant über seine Kommandozeile und seinen dokumentierten Event-Stream. Sie linken die Crates der Engine nicht.

## Konsequenzen

- Die Engine bleibt freie Software. Dieses Dokument ist das Versprechen des Projekts, dass es nicht zu einer proprietären oder Source-Available-Lizenz wechseln wird.
- Ein Programm unter jeder Lizenz kann Volants Events lesen, über `volant-events` oder allein aus dem JSON Schema.
- Ein Schnittstellen-Crate setzt `license = "MIT OR Apache-2.0"` in seiner eigenen `Cargo.toml`, statt die Lizenz des Workspace zu erben, und hält `LICENSE-MIT` und `LICENSE-APACHE` in seinem Verzeichnis. Die `LICENSE` im Wurzelverzeichnis deckt weiterhin den Rest des Repositorys ab.
- Beiträge zu einem Schnittstellen-Crate werden unter MIT OR Apache-2.0 angenommen, bestätigt unter dem Developer Certificate of Origin wie jeder andere Beitrag.
- Ein Schnittstellen-Crate wird für seinen Zweck geschrieben und übernimmt keinen Code aus einem GPL-Crate.
- `volant-protocol` bleibt GPL. Es auf eine andere Lizenz umzustellen, bräuchte die Zustimmung aller, die dazu beigetragen haben.
