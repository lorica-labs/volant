---
sourceHash: 65b041000adc
title: "`become` führt einen zweiten Agent unter `sudo` aus"
description: "Entscheidungsdokument 0004."
sidebar:
  label: "0004 · become unter sudo"
  order: 4
---

| Status | Datum |
|---|---|
| akzeptiert; die Grenze für offene Verbindungen wird durch [ADR 0005](/de/docs/decisions/0005-fork-permits-span-host-local-batches/) abgelöst, und die Methodenliste ist um `su` erweitert (siehe das Update vom 2026-10-24) | 2026-09-09 |

## Kontext

Ansible weitet Rechte pro Task aus und verpackt jeden Modulaufruf in `sudo`. Volant führt Tasks über einen langlebigen Agent pro Host aus, daher würde das Verpacken jedes Tasks entweder einen Agent bedeuten, der für jedes Modul `sudo` aufruft, oder einen Agent, der für das ganze Play selbst privilegiert ist.

## Entscheidung

Der Controller öffnet eine Agent-Verbindung pro (Host, Zielbenutzer). Ein Task mit `become` läuft über den Agent, der als `sudo -H <form> -u <user> -- volant-agent` gestartet wird, wobei `<form>` `-n` oder `-k -S -p ''` ist, wie die unten beschriebene Probe entscheidet; Tasks ohne `become` behalten den unprivilegierten Agent. Nur `sudo` wird unterstützt; andere Methoden werden noch nicht unterstützt und stoppen den Lauf. Der Agent selbst weiß nichts von der Rechteausweitung.

## Konsequenzen

- Ein Wechsel des Zielbenutzers zwischen zwei aufeinanderfolgenden Tasks beendet den Batch; der häufige Fall, ein ganzes Play unter `become`, bleibt bei einem Batch.
- Die Form des `sudo`-Aufrufs wird durch eine Probe festgelegt, nicht dadurch, ob ein Passwort geliefert wurde. Ein `sudo`, das keine Authentifizierung braucht, liest seine Standardeingabe nicht. Auf einem Host mit gecachter Authentifizierung oder einer `NOPASSWD`-Regel bleibt ein dort geschriebenes Passwort also auf der Pipe liegen, und der *Agent* liest es als Header seines ersten Frames. Der Controller fragt daher zuerst `sudo -n` und fällt erst auf `sudo -k -S -p ''` zurück, nachdem `sudo` die Anfrage selbst mangels Authentifizierung abgelehnt hat. Die Form allein aus dem Passwort zu raten lässt die Verbindung hängen, und zwar nur bei der zweiten Verbindung mit erweiterten Rechten innerhalb des Timestamp-Fensters. Deshalb ist die Probe nicht optional.
- Das Passwort, wenn eines angegeben wird, wird einmal auf die Standardeingabe von `sudo` geschrieben und erscheint nie in einer Kommandozeile, einem Log oder einem Ergebnis. Die zwei Typen, die es tragen, haben von Hand geschriebene `Debug`-Implementierungen, die `<redacted>` ausgeben, sodass ein `-vvv`-Dump es nicht preisgeben kann, und die aufgefangene Standardfehlerausgabe von `sudo` wird davon bereinigt, bevor sie eine Meldung erreichen kann.
- Eine fehlgeschlagene Rechteausweitung ist ein fehlgeschlagener Task, kein nicht erreichbarer Host: Die Verbindung hat funktioniert.
- Offene Agent-Verbindungen pro Host sind nicht begrenzt. `forks` begrenzt, wie viele Hosts gleichzeitig laufen, nicht, wie viele Verbindungen ein Host hält, und die Map hält bis zum Recap eine Verbindung pro eigenem Zielbenutzer, jede ein eigener `ssh`-Prozess. Eine Verbindung mit erweiterten Rechten kostet außerdem eine zusätzliche `ssh`-Verbindung für die Probe, zwei, wenn ein Passwort verlangt wird. Diese Entscheidung fügt kein Multiplexing von Verbindungen hinzu: Jede Verbindung ist ein eigener `ssh`-Prozess, und ein breites Inventory bezahlt jede einzeln.
- `su`, `doas`, `pbrun` und der Rest lassen sich als weitere Wege hinzufügen, den Agent zu starten, ohne Protokolländerung.

## Update, 2026-10-24

`su` wird seit 2026-10-16 unterstützt ([#333](https://github.com/lorica-labs/volant/pull/333)), über eine `local`-Verbindung seit 2026-10-23 ([#755](https://github.com/lorica-labs/volant/pull/755)). Es startet den Agent auf dieselbe Weise wie `sudo`, was die Erweiterung ist, die die letzte Konsequenz oben erwartet hat, und braucht keine Protokolländerung. `doas`, `pbrun` und die anderen Methoden werden weiterhin abgelehnt. [Rechteausweitung](/de/docs/hosts/connections/#become) beschreibt beide Methoden.

Die Grenze für offene Verbindungen hat sich nach diesem Dokument zweimal geändert. [ADR 0005](/de/docs/decisions/0005-fork-permits-span-host-local-batches/) band sie an den Fork-Permit. Seit 2026-09-22 ([#178](https://github.com/lorica-labs/volant/pull/178)) hält ein Host seine eigene Verbindung plus höchstens eine Verbindung mit erweiterten Rechten, unabhängig von der Zahl der Zielbenutzer im Play. Eine Rechteausweitung auf einen anderen Benutzer ersetzt die Verbindung mit erweiterten Rechten, die gerade gehalten wird.
