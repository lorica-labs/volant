---
sourceHash: 9fba53e13fc2
title: "Télécharger et décompresser le contenu galaxy dans Volant"
description: "Fiche de décision 0012."
sidebar:
  label: "0012 · HTTP et archives de galaxy"
  order: 12
---

| Statut | Date |
|---|---|
| accepté | 2026-10-27 |

## Contexte

`volant galaxy` installe des collections et des rôles sans l'ansible-galaxy du runtime géré. Pour cela, il parle à un serveur Galaxy en HTTPS, lit et écrit des archives tar gzip, clone des dépôts git et vérifie des signatures GPG. ansible-galaxy 2.19.12 fait les deux premières choses en Python, avec `urllib` et `tarfile`, et lance les programmes `git` et `gpg` pour les deux autres. Mesuré sur 2.19.12 :

- HTTP suit les redirections (téléchargement des artefacts), respecte `https_proxy`, `http_proxy` et `no_proxy`, utilise le magasin de certificats du système, et envoie un en-tête de jeton quand un jeton est configuré.
- `-vvv` affiche `Downloading <url> to <tmp>` et `Validating downloaded file hash <sha256> with expected hash <sha256>`.
- Une archive de collection est extraite fichier par fichier d'après la liste de `FILES.json`, chaque fichier vérifié contre son `chksum_sha256`. Un fichier absent de la liste n'est pas extrait.
- Une source git lance `git`, une signature lance `gpg`.

Avant cette fiche, Volant n'avait ni client HTTP ni code d'archive. Le runtime géré passe par des commandes : `runtime/fetch.rs` lance `curl` (ou `wget`) puis vérifie un SHA-256 épinglé, et `runtime/pack.rs` lance `tar`. Cela suffit pour un fichier épinglé. Galaxy demande plus : un jeton sur les appels d'API qui ne doit pas suivre une redirection vers un autre hôte, du JSON paginé, et une extraction qui n'écrit que ce que liste `FILES.json`.

## Options

| Question | Option | Pour | Contre |
|---|---|---|---|
| HTTP | garder `curl`, comme le runtime | aucune crate nouvelle ; curl lit déjà les variables de proxy et le magasin de certificats du système | un processus par requête ; le JSON et la pagination passent par un tube ; le jeton sur une ligne de commande ou dans un fichier temporaire ; `curl.exe` manque sur les anciens Windows |
| HTTP | `ureq` avec `rustls`, dans le processus | les règles de proxy, de redirection et de jeton sont au même endroit et se comportent pareil sur tous les systèmes ; le `http://` simple vers un serveur de test sur `127.0.0.1` marche tel quel | un binaire plus gros et une compilation plus longue (chiffres plus bas) ; `ring` compile du C, donc une compilation musl demande un compilateur C musl |
| HTTP | les deux, derrière une seule fonction | le runtime garde son chemin testé | deux chemins à tester pour chaque règle, et le chemin curl ne sait toujours pas retirer le jeton lors d'une redirection vers un autre hôte |
| tar et gzip | `tar` du système | rien de nouveau | impossible de vérifier chaque fichier avant de l'écrire ; `tar` GNU et BSD n'ont pas les mêmes options ; `tar.exe` manque sur les anciens Windows |
| tar et gzip | crate `tar` avec `flate2` sur `miniz_oxide` | les entrées arrivent une à une, Volant décide pour chacune s'il l'écrit ; `miniz_oxide` est déjà une dépendance | quelques petites crates |
| git et gpg | programmes externes | ansible-galaxy fait pareil, les erreurs sont donc les mêmes | il faut qu'ils soient installés |

## Décision

### HTTP dans le processus, avec `ureq` et `rustls`

Les requêtes galaxy passent par `ureq` 3, un client HTTP/1.1 bloquant. Le TLS est `rustls` avec le fournisseur `ring`, et les racines viennent du magasin du système par `rustls-native-certs`. Les règles :

- Le proxy vient de l'environnement par `ureq::Proxy::try_from_env`, qui lit `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` et `NO_PROXY`, en majuscules ou en minuscules.
- Les redirections sont suivies. L'en-tête du jeton n'est renvoyé que si la redirection reste sur le même hôte (`RedirectAuthHeaders::SameHost`).
- `--ignore-certs` (`-c`) coupe la vérification des certificats pour l'exécution, comme dans ansible-galaxy.
- L'URL d'un serveur peut être en `http://`, comme dans ansible-galaxy. Le faux serveur Galaxy que les tests lancent sur `127.0.0.1` s'appuie là-dessus.

Le téléchargement du runtime dans `runtime/fetch.rs` garde `curl` pour l'instant. Le passer sur le même client est un changement à part, qui permettrait au contrôleur Windows de télécharger le runtime sans `curl.exe`.

La fonctionnalité `platform-verifier` de `ureq` et sa fonctionnalité par défaut `rustls-webpki-roots` ne sont pas utilisées : toutes deux tirent `webpki-root-certs` ou `webpki-roots`, sous licence CDLA-Permissive-2.0, que `deny.toml` n'accepte pas. `rustls-native-certs` lit le même magasin du système sans elles.

### Les archives avec les crates `tar` et `flate2`

Les archives sont lues et écrites avec la crate `tar`, sans sa fonctionnalité par défaut `xattr`, sur `flate2` avec son `rust_backend`, c'est-à-dire `miniz_oxide`. L'installation d'une collection lit l'archive entrée par entrée et n'écrit une entrée que si `FILES.json` la liste et que son SHA-256 correspond. Une entrée au chemin absolu, avec un composant `..`, ou un lien qui sort de la destination est refusée, comme ansible-galaxy la refuse. La construction d'un artefact de collection écrit l'archive avec les mêmes deux crates.

### `git` et `gpg` restent externes

Une source git lance `git` et une signature lance `gpg`, comme dans ansible-galaxy. Quand l'un manque, Volant affiche ce qu'affiche ansible-galaxy 2.19.12 et sort avec le code 1. Mesuré sans aucun des deux dans le `PATH` :

```text
[ERROR]: Could not find git executable to extract the collection from the Git repository `https://github.com/ansible-collections/ansible.posix.git`: Failed to find required executable "git" in paths: /tmp/emptybin:/sbin:/usr/sbin:/usr/local/sbin
```

```text
[ERROR]: Failed during GnuPG verification with command 'gpg --status-fd=4 --verify --batch --no-tty --no-default-keyring --keyring=/tmp/gtest/k.kbx - /tmp/gtest/cols/ansible_collections/ansible/posix/MANIFEST.json': [Errno 2] No such file or directory: 'gpg'
```

Les chemins du message git sont le `PATH` suivi des répertoires `sbin` qu'ansible-core y ajoute. Galaxy n'utilise pas `curl`, donc un `curl` absent ne le gêne pas.

### Crates à ajouter

Chaque pull request qui ajoute l'une d'elles cite cette fiche.

- `ureq` 3.5, `default-features = false`, fonctionnalité `rustls-no-provider` : le client HTTP, avec les règles de proxy et de redirection ci-dessus.
- `rustls` 0.23, `default-features = false`, fonctionnalités `ring`, `std`, `tls12`, `logging` : le TLS, avec le fournisseur installé une fois au démarrage.
- `rustls-native-certs` 0.8 : le magasin de certificats du système, sans paquet de racines sous licence CDLA.
- `tar` 0.4, `default-features = false` : lit et écrit les entrées tar une à une.
- `flate2` 1.1, `default-features = false`, fonctionnalité `rust_backend` : le gzip sur le `miniz_oxide` dont Volant dépend déjà.

## Conséquences

- Le binaire de publication grossit de 1,6 Mo, soit 13,4 %, mesuré plus bas. Le temps de compilation à vide ne change pas au-delà du bruit d'une exécution à l'autre.

- Chaque règle de galaxy (proxy, jeton, redirection, certificats, extraction) est du code Volant avec ses propres tests, lancés contre le faux serveur Galaxy en HTTP simple.
- Les tâches de publication Linux musl installent déjà `musl-tools` (dist l'ajoute), et les agents sont construits avec zig : le C de `ring` ne demande aucune étape nouvelle à la publication. Une compilation musl locale demande `musl-gcc` ou `cargo zigbuild`.
- Un contrôleur Windows peut installer des collections sans `curl.exe` ni `tar.exe`.

## Mesures, 2026-10-27

`cargo zigbuild --release -p volant --target x86_64-unknown-linux-musl` depuis un répertoire cible vide, sur une machine Linux à 4 cœurs, une exécution chacun. « Avec » ajoute les cinq crates ci-dessus et une fonction qui télécharge une archive et la décompresse, pour que l'éditeur de liens les garde.

| | Sans | Avec | Écart |
|---|---|---|---|
| Binaire allégé | 12 334 584 octets | 13 988 792 octets | +1 654 208 octets (+13,4 %) |
| Compilation à vide | 546 s | 498 s | dans le bruit : la compilation avec les crates est passée en premier et a été la plus rapide |
| `cargo deny check` | passe | passe | deux nouveaux avertissements de versions en double : `getrandom` (0.2, pour `ring`) et `windows-sys` |

La compilation pour la cible musl gagne 21 crates.
