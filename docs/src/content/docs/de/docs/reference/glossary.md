---
sourceHash: 725d6c347d6e
title: Glossar
description: Die Wörter, die diese Dokumentation in einem genauen Sinn verwendet.
---

### Action-Plugin

Code von ansible-core, der auf dem Controller läuft und entscheidet, was für ein Modul wie `copy` oder `package` an einen Host gesendet wird. Volant führt acht davon in Rust aus und den Rest über die Bridge. Siehe [Action-Plugins](/de/docs/internals/action-plugins/).

### Agent

Ein kleines statisches Programm, das Volant auf jeden verwalteten Host hochlädt und dort zwischenspeichert. Es empfängt Tasks vom Controller, führt sie aus und streamt die Ergebnisse zurück. Siehe [Verbindungen und become](/de/docs/hosts/connections/).

### Batch

Die Tasks, die ein Host hintereinander ausführt, bevor er das nächste Mal auf die anderen wartet. Standardmäßig ist ein Batch einen Task breit. Siehe [Host-übergreifendes Batching](/de/docs/hosts/batching/).

### Bridge

Der Weg, den ein Action-Plugin nimmt, wenn Volant keine Rust-Version davon hat: Das eigene Python des Plugins aus ansible-core läuft im Helfer des Controllers, und der Controller beantwortet, was es anfragt. Siehe [On the bridge](/de/docs/internals/action-plugins/#auf-der-bridge).

### Controller

Der Rechner, auf dem Sie `volant` ausführen, und das Programm `volant` selbst. Er liest Playbooks, rendert Templates und steuert die Agents.

### Flush-Punkt

Ein Schritt, an dem benachrichtigte Handler laufen. Jedes Play hat drei, und `meta: flush_handlers` fügt weitere hinzu. Siehe [Handler](/de/docs/playbooks/blocks/#handler).

### Verbindung

Eine offene Verbindung vom Controller zu einem Agent. Ein Host hat seine eigene, dazu eine, solange er die Rechte ausweitet, und eine pro Delegationsziel.

### Verwaltete Runtime

Das Python und ansible-core, die Volant für sich selbst installiert und vor jedem Interpreter des Systems verwendet. Siehe [Die verwaltete Runtime](/de/docs/reference/runtime/).

### Natives Modul

Eine Rust-Version eines Moduls, die der Agent mitbringt. Sie beantwortet einen Task selbst, wenn dessen Argumente innerhalb dessen bleiben, was sie umsetzt, und gibt den Task sonst an das Python-Modul zurück. Siehe [Native Module](/de/docs/internals/python/#native-module).

### Vorabprüfung

Die Prüfung, die vor jeder Verbindung alles benennt, was dieses Release noch nicht unterstützt, und den Lauf stoppt. Siehe [Was noch nicht unterstützt wird](/de/docs/internals/preflight/).

### Die Referenz

ansible-core 2.19.12, das Release, dessen Verhalten Volant nachbildet. Eine Abweichung davon ist ein Bug, sofern keine Seite sie als gewollt aufführt. Siehe [Entscheidungsdokument 0002](/de/docs/decisions/0002-ansible-core-2-19-as-reference/).

### Einfügepunkt

Ein Schritt, dessen Nachfolger erst bekannt sind, während das Play läuft: ein Handler-Flush oder ein dynamisches `include_tasks` oder `include_role`. Die neuen Schritte werden dort eingefügt. Siehe [Imports und Includes](/de/docs/playbooks/roles/#imports-und-includes).

### Schritt

Ein Eintrag der flachen, nummerierten Liste, in die ein Play kompiliert wird. Jeder Host durchläuft dieselbe Liste. Siehe [Plays ausführen](/de/docs/playbooks/how-a-play-runs/).

### Synchronisationspunkt

Ein Schritt, an dem jeder Host eines Batches auf die anderen wartet. Beim Standardverhalten `linear` ist jeder Task einer.

### Union

Die Menge der Module, die ein Lauf an einen Host senden darf, einmal vor der ersten Verbindung aufgebaut. Siehe [Der warme Python-Pfad](/de/docs/internals/python/).

### Union-Blob

Das eine Archiv, das jedes Python-Modul enthält, das ein Lauf braucht, samt dem gemeinsamen Code, den sie importieren. Es wird einmal an jeden Host gesendet. Siehe [Der warme Python-Pfad](/de/docs/internals/python/).

### Warmer Python-Server

Ein Python-Prozess, den der Agent auf einem Host laufen lässt. Er lädt den gemeinsamen Modulcode einmal und forkt für jeden Task ein Kind.
