---
sourceHash: d52d376402f0
title: "`ansible_version` meldet das Referenz-Release von ansible-core"
description: "Entscheidungsdokument 0003."
sidebar:
  label: "0003 · ansible_version"
  order: 3
---

| Status | Datum |
|---|---|
| akzeptiert | 2026-09-06 |

## Kontext

Viele Galaxy-Rollen verzweigen auf `ansible_version.full` oder `ansible_version.major`, um einen Codepfad zu wählen. Volant ist nicht Ansible, bildet aber das Verhalten eines ansible-core-Releases nach (siehe [ADR 0002](/de/docs/decisions/0002-ansible-core-2-19-as-reference/)). `ansible_version` undefiniert zu lassen, würde diese Rollen an einer undefinierten Variable scheitern lassen; einen Volant-eigenen Wert zu erfinden, würde sie auf ungetestete Pfade schicken.

## Entscheidung

`ansible_version` hält das Referenz-Release, das Volant nachbildet, in der Dictionary-Form, die Rollen erwarten (`full`, `major`, `minor`, `revision`, `string`). Eine separate Variable `volant_version` hält Volants eigene Version.

## Konsequenzen

- Rollen, die für das Referenz-Release geschrieben sind, verhalten sich wie darunter.
- Wenn die Referenz wandert (Vorgehen aus [ADR 0002](/de/docs/decisions/0002-ansible-core-2-19-as-reference/)), wandert `ansible_version` mit, in derselben Änderung.
- Ein Playbook, das Volant von Ansible unterscheiden muss, prüft `volant_version is defined`.
