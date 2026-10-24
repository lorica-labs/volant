---
sourceHash: 71d56473bdd4
title: "Fork-Permits gelten über host-lokale Batches hinweg"
description: "Entscheidungsdokument 0005."
sidebar:
  label: "0005 · Fork-Permits und Batching"
  order: 5
---

| Status | Datum |
|---|---|
| akzeptiert und seit 2026-09-20 von einer Option abhängig (siehe Status unten); die Grenze für Verbindungen mit erweiterten Rechten wird durch das Update vom 2026-10-24 abgelöst | 2026-09-16 |

## Kontext

Ein Host-Treiber nimmt einen von `forks` Permits, bevor er eine Verbindung öffnet, und gibt ihn zurück, wenn der gesammelte Batch beantwortet ist. [ADR 0004](/de/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) ließ offene Agent-Verbindungen unbegrenzt; die Verbindungen mit erweiterten Rechten an den Permit zu binden, begrenzt sie, denn `forks` limitiert dann die offenen Verbindungen ebenso wie die arbeitenden Hosts. Zwanzig Hosts, die unter `ulimit -n 128` und `-f 5` auf root ausweiteten, meldeten früher `Too many open files` für einen Host, der völlig erreichbar war.

Die Verbindungen bei jedem Batch zurückzugeben ist nicht umsonst, und ein Batch endet öfter, als das Wort vermuten lässt. Jedes davon schließt einen: ein Task mit `register`, `loop`, `changed_when` oder `failed_when`; ein Task, der den Zustand eines anderen Hosts liest, was eine Grenze vor ihm selbst ist; und ein Task, der auf einen anderen Benutzer ausweitet. Ein Ergebnis zu registrieren ist gewöhnlich, daher bezahlte ein Play mit sechs Tasks mit erweiterten Rechten, jeder mit einem `register`, sechs Rechteausweitungen, jede eine `sudo`-Probe und ein `sudo`, das den Agent startet. Gemessen auf dem Entwicklungsrechner mit einem `sudo`, das aufzeichnet, was es ausführen soll: zwölf Aufrufe für diese sechs Tasks.

Nichts an einem `register` braucht einen anderen Host, der Permit ging also vor einem Warten zurück, das es nicht gab.

## Entscheidung

Ein Treiber behält seinen Fork-Permit und seine Verbindungen mit erweiterten Rechten von einem Batch zum nächsten, wenn er gleich mit einem Schritt seines eigenen weitermacht. Er gibt beide vor jedem Warten auf die anderen Hosts zurück und am Ende des Plays, wo das Halten nur Deskriptoren belegen würde. Ein Treiber, der den Lauf durch einen Fehler verlässt, gibt sie ebenfalls zurück.

Vier Schritte zählen als Warten: einer, den die Keyword-Tabelle zum Synchronisationspunkt erklärt, einer, dessen Text den Zustand eines anderen Hosts nennt, ein Einfügepunkt, an dem die Schrittliste wächst, und ein Schritt, dessen Nachfolger hinter einem solchen liegt. Das gilt auch für einen Schritt, der zurückgehalten wird, bis sein eigenes Ergebnis entscheidet, wohin der Host geht.

Verbindungen mit erweiterten Rechten werden wie bisher abseits des Tasks geschlossen statt abgewartet: `shutdown` gewährt seinem Agent bis zu zwei Sekunden, und ein Treiber, der das zwischen Batches bezahlt, würde serialisieren, was das Freigeben des Permits gerade frei gemacht hat.

Diese Entscheidung fügt kein Multiplexing von Verbindungen hinzu. Jede Verbindung bleibt ein eigener `ssh`-Prozess.

## Konsequenzen

- Offene Verbindungen sind begrenzt auf eine pro Host, für die Verbindung, die das Play überdauert, plus `forks` mal die Zahl der verschiedenen Zielbenutzer, auf die ein Treiber ausweitet, während er einen Permit hält. Die frühere Grenze zählte eine Verbindung mit erweiterten Rechten pro Treiber, weil jeder Batch seine eigene schloss; mehrere aufeinanderfolgende Batches unter einem Permit können jetzt verschiedene Benutzer nennen.
- Gemessen, zwanzig Hosts bei `-f 5`, die auf root ausweiten: Der Controller erreicht 84 offene Deskriptoren, wo er vorher 69 erreichte, bei einem Limit von 128. Der Lauf endet mit 0 ohne nicht erreichbaren Host, derselbe Nachweis, auf dem die frühere Grenze akzeptiert wurde.
- Ein Play mit sechs Tasks mit erweiterten Rechten, von denen jeder seinen Batch mit einem `register` schließt, startet einen Agent mit erweiterten Rechten statt sechs.
- Hosts kommen an einem Warten an die Reihe statt bei jedem Batch. Mit `forks` unter der Zahl der aktiven Hosts und einem Play, dessen Schritte alle host-lokal sind, führt ein Host jetzt seine restlichen Schritte aus, bevor der nächste startet, wo beide früher abwechselten. Die Arbeit und der Recap sind gleich; was sich ändert, ist die Reihenfolge, in der Zeilen ankommen, die zwischen zwei Hosts ohne etwas, das sie synchronisiert, ohnehin ungeordnet war.
- Ein Play, das nie die Rechte ausweitet, bleibt davon unberührt, außer dass es seinen Permit etwas länger hält, und ein Play, dessen Tasks alle über Hosts hinweg lesen, gibt den Permit bei jedem Schritt zurück, wie bisher.

Löst die Konsequenz „Offene Agent-Verbindungen pro Host sind nicht begrenzt“ aus [ADR 0004](/de/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) ab.

## Status

Seit 2026-09-20 trägt ein Treiber seinen Permit nur dann über einen host-lokalen Schritt hinaus, wenn der Operator darum bittet. `[volant] batching = true` in `ansible.cfg` oder `VOLANT_BATCHING=1` schaltet diese Entscheidung ein; ohne sie ist jeder Schritt ein Warten, wie `linear` es sagt.

Der Grund ist die vierte Konsequenz oben. Welche Schritte als Warten zählen, wird aus dem Text eines Tasks gelesen, und eine Abhängigkeit, die über eine Datei, eine Datenbank oder einen Dienst läuft, hinterlässt keinen Text zum Lesen. Das `linear` von Ansible verspricht, dass die Hosts sich vor jedem Task treffen, und ein Playbook, das gegen dieses Versprechen geschrieben wurde, darf sich darauf verlassen. Allein aus dem Text anders zu entscheiden war der falsche Standard für eine Engine, deren erstes Versprechen ist, dass Playbooks unverändert laufen.

Alles, was die Entscheidung beschreibt, bleibt erhalten und läuft weiter unter der Option. Die gemessenen Kosten der beiden Einstellungen und was der Standard in jedem Fall behält, stehen unter [Host-übergreifendes Batching](/de/docs/hosts/batching/).

## Update, 2026-10-24

Die erste Konsequenz oben gilt nicht mehr. Seit 2026-09-22 ([#178](https://github.com/lorica-labs/volant/pull/178)) behält ein Host seine Verbindung mit erweiterten Rechten über einen Synchronisationspunkt hinweg, und er hält seine eigene Verbindung plus höchstens eine Verbindung mit erweiterten Rechten, unabhängig von der Zahl der verschiedenen Zielbenutzer. Ein Batch, der anders ausweitet als der gehaltene (ein anderer Benutzer, ein anderes Programm oder andere Flags für die Rechteausweitung, ein anderer Transport), schließt die alte Verbindung und öffnet eine neue. Der Permit selbst geht weiterhin vor jedem Warten zurück, wie hier entschieden. [Verbindungen und become](/de/docs/hosts/connections/#dauerhafte-verbindungen) nennt die aktuelle Regel.
