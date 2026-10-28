---
sourceHash: 75fc765d3e37
title: Die verwaltete Runtime
description: Das Python und ansible-core, die Volant für sich selbst installiert, wo sie liegen, was sie herunterladen und wie Sie sie verwalten.
---

Volant braucht auf dem Controller ein Python 3 mit `ansible-core` für Python-Module, das Sammeln von Facts und die Filter und Lookups, die es nicht selbst umsetzt. Hat der Rechner keines, installiert Volant sein eigenes, einmal, und verwendet es von da an. Sie müssen ansible nicht installiert haben, um ein Playbook auszuführen, und auch nicht für `volant doc`, `config`, `galaxy`, `pull` und `console`, die die eigenen Werkzeuge von ansible-core aus dieser Runtime ausführen.

Die Runtime ist CPython 3.13 und ansible-core 2.19.12, die Version, der Volant entspricht. Sie liegt in einem einzigen Verzeichnis, das Volant gehört, sodass sie weder ein System-Python noch eine virtuelle Umgebung von Ihnen berührt.

## Wann sie installiert wird

Ein Lauf installiert die Runtime, wenn er den Python-Helfer braucht und keinen verwendbaren Interpreter findet. Er gibt eine Zeile auf der Standardfehlerausgabe aus und macht weiter:

```text
Installing Volant's Python runtime (cpython-3.13.16-ansible-core-2.19.12-98a5d8b2) ...
```

Ein Lauf, der nur Facts sammelt, installiert nie etwas. Sie können die Runtime auch vorab installieren, zum Beispiel in einem Image:

```sh
volant runtime install
```

Auf einem frischen Rechner mit normaler Verbindung dauerte die Installation etwa 8 Sekunden. Spätere Läufe finden sie mit zwei Dateiprüfungen.

## Welchen Interpreter ein Lauf verwendet

Der erste, der zutrifft:

1. `VOLANT_PYTHON`, wenn es gesetzt ist. Volant verwendet genau diesen Interpreter und installiert nie.
2. Die verwaltete Runtime, wenn sie installiert ist.
3. Die aktive virtuelle Umgebung, dann `python3` in Ihrem `PATH`, wenn sie ansible-core haben.
4. Eine neue verwaltete Runtime, jetzt installiert.

Volant startet den Interpreter mit `-P`, sodass eine Datei im Arbeitsverzeichnis, etwa `ansible.py` oder `json.py`, nie anstelle des echten Moduls importiert wird. `-P` braucht Python 3.11 oder neuer, was auch ansible-core 2.19 verlangt. Ein eigener Interpreter behält `PYTHONPATH` und seine User-Site. [`volant doc`](/de/docs/reference/cli/doc/) wählt seinen Interpreter ebenfalls nach dieser Regel und führt darin das eigene `ansible-doc` von ansible-core aus.

`volant runtime status` zeigt die Wahl, die der nächste Lauf treffen würde, mit jedem Kandidaten und seiner Herkunft. Die [Einstellung `runtime`](/de/docs/reference/configuration/#volant) ändert diese Reihenfolge.

## Was sie herunterlädt

Volant holt einen eigenständigen CPython-Build und die ansible-core-Wheels mit `curl`, oder mit `wget`, wenn `curl` fehlt. Außerdem führt es `tar` aus. Die URLs und der SHA-256 jeder Datei sind in die Volant-Binärdatei fest eingetragen, sodass jedes Release genau weiß, welche Dateien es verwenden darf.

- Jeder Download wird gegen seinen eingetragenen Hash geprüft, bevor irgendetwas daraus läuft. Bei einer Abweichung löscht Volant den Download und hält mit beiden Hashes an.
- `pip` installiert nur die eingetragenen Wheels, mit `--require-hashes` und ohne Index.
- Die Installation geschieht in einem temporären Verzeichnis neben dem endgültigen und wird an ihren Platz verschoben, nachdem geprüft wurde, dass `import ansible` funktioniert. Eine fehlgeschlagene Installation entfernt ihr temporäres Verzeichnis. Eine unterbrochene kann ein Verzeichnis `.<id>.partial-*` hinterlassen, das die nächste Installation löscht.
- Eine Lock-Datei verhindert, dass zwei Volant-Prozesse gleichzeitig installieren. Der zweite wartet und findet dann die installierte Runtime.
- Jeder Kindprozess läuft mit `PYTHONNOUSERSITE=1` und ohne `PYTHONPATH`, sodass Pakete in Ihrem Home-Verzeichnis oder Ihrer Umgebung ihn nicht erreichen können.

Die Plattformen sind Linux und macOS auf x86_64 und arm64. Unter Linux liest Volant die libc des Rechners, auf dem es läuft, glibc oder musl, und wählt den passenden Build, wofür es auch kompiliert wurde. Volant wurde auf macOS nicht getestet: Es stand kein Mac zur Verfügung. Jedes andere System wird mit einer Meldung abgelehnt, die es nennt und auf `VOLANT_PYTHON` verweist.

## Wo sie liegt

Unter `$XDG_DATA_HOME/volant/runtime` oder `~/.local/share/volant/runtime`. Unter macOS ist der Standard `~/Library/Application Support/volant/runtime`. Jede Runtime hat ihr eigenes Verzeichnis, benannt nach ihrer Kennung (Python-Version, ansible-core-Version und ein kurzer Hash der eingetragenen Dateiliste), sodass ein Release mit anderen Einträgen neben der alten installiert wird. Eine Runtime belegt etwa 155 MB.

## Collections

Volant installiert während eines Laufs keine Collection. So installieren Sie eine in die Runtime:

```sh
volant runtime install --collections community.general ansible.posix:2.2.2
volant runtime install --collections requirements.yml
```

`--collections` nimmt Namen, jeden mit optionalem `:version`, oder eine Requirements-Datei, die auf `.yml` oder `.yaml` endet. Die Collections landen in `collections/` innerhalb der Runtime. Volant sucht dort nach den Pfaden in `ANSIBLE_COLLECTIONS_PATH` und `[defaults] collections_path`, sodass Collections, die Sie selbst installiert haben, gewinnen. [`volant galaxy`](/de/docs/reference/cli/galaxy/) installiert in den ersten Eintrag dieses Pfads, standardmäßig `~/.ansible/collections`, und listet das Verzeichnis der Runtime zuletzt.

Die Installation ist Volants eigene: `volant runtime install --collections` führt `volant galaxy collection install -p <runtime>/collections` aus, gibt dieselben Zeilen aus und braucht für diesen Schritt kein Python.

Die Runtime enthält ansible-core und seine Abhängigkeiten und sonst nichts. Eine Collection, deren Plugins ein weiteres Python-Paket brauchen, etwa `jmespath` für `json_query`, funktioniert darin nicht. Nutzen Sie dafür Ihre eigene Umgebung mit `VOLANT_PYTHON`.

## Befehle

| Befehl | Was er tut |
|---|---|
| `volant runtime install` | Installiert die Runtime, wenn sie fehlt. |
| `volant runtime install --collections NAME_OR_FILE...` | Installiert zusätzlich Collections. |
| `volant runtime install --from FILE` | Installiert aus einer Datei, die `pack` erzeugt hat, ohne Download. |
| `volant runtime install --from FILE --sha256 HASH` | Installiert aus der Datei nur, wenn ihr SHA-256 `HASH` ist. |
| `volant runtime pack FILE` | Schreibt die installierte Runtime in eine Datei und gibt ihren SHA-256 aus. |
| `volant runtime status` | Zeigt den Modus, den Interpreter, den der nächste Lauf verwendet, und die verwaltete Runtime. |
| `volant runtime path` | Gibt das Python der verwalteten Runtime aus. Endet mit Exit-Code 1, wenn sie nicht installiert ist. |
| `volant runtime remove [ID]` | Löscht die aktuelle Runtime oder die genannte. |
| `volant runtime remove --all` | Löscht jede Runtime. |

### Rechner ohne Netzwerk

Packen Sie eine Runtime auf einem Rechner mit Netzwerk, kopieren Sie die Datei und installieren Sie sie auf einem Rechner derselben Plattform:

```sh
volant runtime pack runtime.tar.gz
# packed runtime.tar.gz
# sha256 3f1c...e9
volant runtime install --from runtime.tar.gz --sha256 3f1c...e9
```

`install --from` lehnt eine Datei ab, die für eine andere Plattform oder eine andere Runtime gemacht wurde, und nennt beide. `pack` gibt den SHA-256 der Datei in einer Zeile `sha256` aus. Mit `--sha256 HASH` lehnt `install --from` eine Datei mit abweichendem Hash vor dem Entpacken ab und zeigt beide Hashes. Ohne die Option gibt die Installation den Hash der verwendeten Datei aus, sodass Sie ihn selbst vergleichen können. Das Installieren eines Packs führt das darin enthaltene Python aus, übergeben Sie also den Hash, den Sie von dem Rechner erhalten haben, der es erzeugt hat.

## Ausschalten

Setzen Sie `runtime = system` im Abschnitt `[volant]` der `ansible.cfg` oder `VOLANT_RUNTIME=system`. Volant verwendet dann nur, was vorher vorhanden war, und ein fehlender Interpreter wird mit der alten Meldung plus einem Hinweis auf die Runtime abgelehnt. Mit `managed` verwendet Volant nur `VOLANT_PYTHON` oder die verwaltete Runtime, installiert die Runtime, wenn sie fehlt, und ignoriert `python3` im `PATH`. Siehe [Konfiguration](/de/docs/reference/configuration/#volant).
