---
sourceHash: 9fba53e13fc2
title: "Galaxy-Inhalte in Volant selbst laden und entpacken"
description: "Entscheidungsprotokoll 0012."
sidebar:
  label: "0012 · Galaxy, HTTP und Archive"
  order: 12
---

| Status | Datum |
|---|---|
| akzeptiert | 2026-10-27 |

## Kontext

`volant galaxy` installiert Collections und Rollen ohne das ansible-galaxy der verwalteten Laufzeit. Dafür spricht es über HTTPS mit einem Galaxy-Server, liest und schreibt gzip-Tarballs, klont git-Repositories und prüft GPG-Signaturen. ansible-galaxy 2.19.12 erledigt die ersten beiden Aufgaben in Python, mit `urllib` und `tarfile`, und ruft für die anderen beiden die Programme `git` und `gpg` auf. Gemessen mit 2.19.12:

- HTTP folgt Weiterleitungen (Download der Artefakte), beachtet `https_proxy`, `http_proxy` und `no_proxy`, nutzt den CA-Speicher des Systems und sendet einen Token-Header, wenn ein Token konfiguriert ist.
- `-vvv` gibt `Downloading <url> to <tmp>` und `Validating downloaded file hash <sha256> with expected hash <sha256>` aus.
- Ein Collection-Tarball wird Datei für Datei nach der Liste in `FILES.json` entpackt, jede Datei gegen ihre `chksum_sha256` geprüft. Eine Datei, die nicht in der Liste steht, wird nicht entpackt.
- Eine git-Quelle ruft `git` auf, eine Signatur ruft `gpg` auf.

Vor diesem Protokoll hatte Volant weder einen HTTP-Client noch Archivcode. Die verwaltete Laufzeit ruft Programme auf: `runtime/fetch.rs` startet `curl` (oder `wget`) und prüft danach einen festgelegten SHA-256, `runtime/pack.rs` startet `tar`. Für eine festgelegte Datei reicht das. Galaxy braucht mehr: ein Token bei API-Aufrufen, das einer Weiterleitung auf einen anderen Host nicht folgen darf, seitenweises JSON und ein Entpacken, das nur schreibt, was `FILES.json` auflistet.

## Optionen

| Frage | Option | Dafür | Dagegen |
|---|---|---|---|
| HTTP | `curl` behalten, wie die Laufzeit | keine neue Crate; curl liest die Proxy-Variablen und den CA-Speicher des Systems schon | ein Prozess pro Anfrage; JSON und Seiten laufen über eine Pipe; das Token steht in einer Befehlszeile oder einer temporären Datei; `curl.exe` fehlt auf älteren Windows-Versionen |
| HTTP | `ureq` mit `rustls`, im Prozess | Proxy-, Weiterleitungs- und Token-Regeln stehen an einer Stelle und verhalten sich auf jedem System gleich; einfaches `http://` zu einem Testserver auf `127.0.0.1` funktioniert direkt | ein größeres Binary und ein längerer Build (Zahlen unten); `ring` kompiliert C, ein musl-Build braucht also einen musl-C-Compiler |
| HTTP | beides, hinter einer Funktion | die Laufzeit behält ihren getesteten Weg | zwei Wege, die für jede Regel getestet werden müssen, und der curl-Weg kann das Token bei einer Weiterleitung auf einen anderen Host weiterhin nicht entfernen |
| tar und gzip | `tar` des Systems | nichts Neues | jede Datei vor dem Schreiben zu prüfen ist nicht möglich; GNU- und BSD-`tar` haben unterschiedliche Optionen; `tar.exe` fehlt auf älteren Windows-Versionen |
| tar und gzip | Crate `tar` mit `flate2` auf `miniz_oxide` | die Einträge kommen einzeln, Volant entscheidet für jeden, ob er geschrieben wird; `miniz_oxide` ist schon eine Abhängigkeit | ein paar kleine Crates |
| git und gpg | externe Programme | ansible-galaxy macht es genauso, die Fehler sind also dieselben | sie müssen installiert sein |

## Entscheidung

### HTTP im Prozess, mit `ureq` und `rustls`

Galaxy-Anfragen laufen über `ureq` 3, einen blockierenden HTTP/1.1-Client. TLS ist `rustls` mit dem Provider `ring`, die Wurzelzertifikate kommen über `rustls-native-certs` aus dem Speicher des Systems. Die Regeln:

- Der Proxy kommt über `ureq::Proxy::try_from_env` aus der Umgebung, das `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` und `NO_PROXY` liest, in Groß- oder Kleinschreibung.
- Weiterleitungen werden verfolgt. Der Token-Header wird nur erneut gesendet, wenn die Weiterleitung auf demselben Host bleibt (`RedirectAuthHeaders::SameHost`).
- `--ignore-certs` (`-c`) schaltet die Zertifikatsprüfung für den Lauf ab, wie in ansible-galaxy.
- Eine Server-URL darf `http://` sein, wie in ansible-galaxy. Der falsche Galaxy-Server, den die Tests auf `127.0.0.1` starten, verlässt sich darauf.

Der Download der Laufzeit in `runtime/fetch.rs` behält vorerst `curl`. Ihn auf denselben Client umzustellen ist eine eigene Änderung; danach könnte der Windows-Controller die Laufzeit ohne `curl.exe` laden.

Das Feature `platform-verifier` von `ureq` und sein Standard-Feature `rustls-webpki-roots` werden nicht genutzt: Beide ziehen `webpki-root-certs` oder `webpki-roots` nach sich, lizenziert unter CDLA-Permissive-2.0, die `deny.toml` nicht zulässt. `rustls-native-certs` liest denselben Speicher des Systems ohne sie.

### Archive mit den Crates `tar` und `flate2`

Archive werden mit der Crate `tar` gelesen und geschrieben, ohne ihr Standard-Feature `xattr`, über `flate2` mit dessen `rust_backend`, also `miniz_oxide`. Beim Installieren einer Collection wird der Tarball Eintrag für Eintrag gelesen, und ein Eintrag wird nur geschrieben, wenn `FILES.json` ihn auflistet und sein SHA-256 passt. Ein Eintrag mit absolutem Pfad, mit einer Komponente `..` oder mit einem Link, der aus dem Ziel hinausführt, wird abgelehnt, wie ansible-galaxy ihn ablehnt. Das Bauen eines Collection-Artefakts schreibt den Tarball mit denselben beiden Crates.

### `git` und `gpg` bleiben extern

Eine git-Quelle ruft `git` auf und eine Signatur ruft `gpg` auf, wie in ansible-galaxy. Fehlt eines davon, gibt Volant aus, was ansible-galaxy 2.19.12 ausgibt, und endet mit Code 1. Gemessen ohne beide Programme im `PATH`:

```text
[ERROR]: Could not find git executable to extract the collection from the Git repository `https://github.com/ansible-collections/ansible.posix.git`: Failed to find required executable "git" in paths: /tmp/emptybin:/sbin:/usr/sbin:/usr/local/sbin
```

```text
[ERROR]: Failed during GnuPG verification with command 'gpg --status-fd=4 --verify --batch --no-tty --no-default-keyring --keyring=/tmp/gtest/k.kbx - /tmp/gtest/cols/ansible_collections/ansible/posix/MANIFEST.json': [Errno 2] No such file or directory: 'gpg'
```

Die Pfade in der git-Meldung sind der `PATH`, gefolgt von den `sbin`-Verzeichnissen, die ansible-core anhängt. Galaxy nutzt `curl` nicht, ein fehlendes `curl` stört es also nicht.

### Hinzuzufügende Crates

Jeder Pull Request, der eine davon hinzufügt, verweist auf dieses Protokoll.

- `ureq` 3.5, `default-features = false`, Feature `rustls-no-provider`: der HTTP-Client, mit den Proxy- und Weiterleitungsregeln oben.
- `rustls` 0.23, `default-features = false`, Features `ring`, `std`, `tls12`, `logging`: TLS, der Provider wird einmal beim Start installiert.
- `rustls-native-certs` 0.8: der CA-Speicher des Systems, ohne CDLA-lizenziertes Wurzelpaket.
- `tar` 0.4, `default-features = false`: liest und schreibt tar-Einträge einzeln.
- `flate2` 1.1, `default-features = false`, Feature `rust_backend`: gzip über das `miniz_oxide`, von dem Volant schon abhängt.

## Konsequenzen

- Das Release-Binary wird um 1,6 MB größer, 13,4 %, gemessen unten. Die Dauer eines Builds von Grund auf ändert sich nicht über das Rauschen zwischen Läufen hinaus.

- Jede Galaxy-Regel (Proxy, Token, Weiterleitung, Zertifikate, Entpacken) ist Volant-Code mit eigenen Tests, die gegen den falschen Galaxy-Server über einfaches HTTP laufen.
- Die Linux-musl-Release-Jobs installieren `musl-tools` schon (dist fügt es hinzu), und die Agents werden mit zig gebaut: Der C-Code von `ring` braucht keinen neuen Schritt im Release. Ein lokaler musl-Build braucht `musl-gcc` oder `cargo zigbuild`.
- Ein Windows-Controller kann Collections ohne `curl.exe` oder `tar.exe` installieren.

## Messungen, 2026-10-27

`cargo zigbuild --release -p volant --target x86_64-unknown-linux-musl` aus einem leeren Zielverzeichnis, auf einer Linux-Maschine mit 4 Kernen, je ein Lauf. „Mit“ fügt die fünf Crates oben hinzu und eine Funktion, die einen Tarball lädt und entpackt, damit der Linker sie behält.

| | Ohne | Mit | Unterschied |
|---|---|---|---|
| Gestripptes Binary | 12.334.584 Bytes | 13.988.792 Bytes | +1.654.208 Bytes (+13,4 %) |
| Build von Grund auf | 546 s | 498 s | im Rauschen: der Build mit den Crates lief zuerst und war der schnellere |
| `cargo deny check` | besteht | besteht | zwei neue Warnungen zu doppelten Versionen: `getrandom` (0.2, für `ring`) und `windows-sys` |

Der Build für das musl-Ziel bekommt 21 Crates dazu.
