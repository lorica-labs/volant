---
sourceHash: c65d7e53b4fd
title: Vault
description: Comment Volant lit les données d'Ansible Vault, d'où il tire les mots de passe, et pourquoi ils ne quittent jamais le processus du contrôleur.
---

Volant lit et écrit les formats vault 1.1 et 1.2 d'ansible-core 2.19.12. Les résultats, les messages d'erreur et les codes de sortie correspondent à ceux d'ansible-core.

## D'où viennent les mots de passe

| Source | Remarques |
|---|---|
| `-J`, `--ask-vault-pass` | Demandé sur le terminal, sans écho. |
| `--vault-password-file`, `--vault-pass-file` | Un fichier, ou un exécutable qui affiche le mot de passe. Répétable. |
| `--vault-id LABEL@SOURCE` | La source est un fichier, un exécutable ou `prompt`. Répétable. |
| `[defaults] vault_password_file`, `vault_identity_list` | La même chose, depuis `ansible.cfg`. |
| `ANSIBLE_VAULT_PASSWORD_FILE`, `ANSIBLE_VAULT_IDENTITY_LIST` | La même chose, depuis l'environnement. |

Chaque mot de passe est essayé sur chaque valeur chiffrée, quel que soit son label.

## Ce qui est déchiffré

- Les valeurs `!vault` en ligne dans un playbook ou un fichier de variables. Elles se chargent sous forme chiffrée et se déchiffrent quand un modèle les lit. Une valeur déchiffrée garde la confiance qu'ansible-core lui accorde.
- Les fichiers entièrement chiffrés : `vars_files`, `include_vars`, et les fichiers `group_vars` et `host_vars`. Un fichier chiffré qui ne peut pas être lu échoue avec le message d'ansible-core.
- Les filtres `vault` et `unvault`, les tests `vault_encrypted` et `vaulted_file`, et le lookup `unvault`.
- Un `src` chiffré de `copy`, `template` et `script`, et une archive chiffrée d'`unarchive`. L'exécution la déchiffre dans un répertoire temporaire privé avant que le plugin ne la lise. `decrypt: false` sur `copy` ou `unarchive` garde le texte chiffré.

Une valeur `!vault` qu'aucun secret n'ouvre fait échouer la tâche qui la lit, avec les trois blocs qu'ansible-core affiche. `debug: var=x` se termine avec 2, avec la cause à la ligne où la valeur est écrite. `is vault_encrypted` répond pour les variables de l'hôte courant.

## Les secrets restent sur le contrôleur

Un mot de passe vault vit en mémoire dans le processus du contrôleur. Il n'est jamais écrit dans une ligne de commande, dans l'environnement d'un processus fils, dans un journal, dans un résultat ni dans le lien avec l'agent. Les mots de passe de connexion et d'élévation de privilèges suivent la même règle, et [Connexions](/fr/docs/hosts/connections/#types-de-connexion) explique comment le mot de passe SSH parvient à `ssh`.

Les filtres, tests et lookups qui s'exécutent en Python ne voient jamais le mot de passe vault. Ceux de vault s'exécutent pour cette raison dans le processus du contrôleur. Voir [Filtres, tests et lookups qui nécessitent Python](/fr/docs/variables/templating/#filtres-tests-et-lookups-qui-nécessitent-python).

Une valeur revenue d'un hôte géré n'est jamais déchiffrée avec les secrets de l'exécution, et n'est jamais rendue de nouveau comme modèle. Voir [Les valeurs venues des hôtes ne sont jamais rendues comme modèles](/fr/docs/variables/templating/#les-valeurs-venues-des-hôtes-ne-sont-jamais-rendues-comme-modèles).

## Chiffrer et afficher des fichiers

`volant vault encrypt`, `decrypt` et `view` travaillent sur les mêmes fichiers qu'`ansible-vault` : un fichier chiffré par l'un est lu par l'autre, avec l'id par défaut comme avec un label.

- `encrypt` remplace chaque fichier par son texte vault et `decrypt` par son texte en clair. Les deux laissent le résultat en mode `0600` et n'affichent rien ; sur un terminal, ils écrivent `Encryption successful` ou `Decryption successful` sur l'erreur standard. Un fichier sans saut de ligne final revient sans saut de ligne.
- L'en-tête est `$ANSIBLE_VAULT;1.1;AES256` pour l'id par défaut et `$ANSIBLE_VAULT;1.2;AES256;LABEL` pour un autre. `--vault-id LABEL@SOURCE` fixe le label. Avec plusieurs `--vault-id`, `--encrypt-vault-id` (ou `vault_encrypt_identity`, `ANSIBLE_VAULT_ENCRYPT_IDENTITY`) choisit celui qui chiffre ; sans lui, la commande sort avec le code 5.
- `--output FILE` écrit à cet endroit et laisse la source intacte, `--output -` écrit sur la sortie standard, et sans nom de fichier, ou avec `-`, la commande lit l'entrée standard.
- `view` affiche le texte en clair de chaque fichier. Sur un terminal, il passe par le pager : `PAGER`, sinon `ANSIBLE_PAGER`, sinon `[defaults] pager`, sinon `less`.
- Sans aucune source de mot de passe, la commande le demande. `encrypt` le demande deux fois, comme `New Vault password: ` et `Confirm New Vault password: `.
- `-v` affiche le fichier de configuration utilisé et `-vv` la version. Les lignes supplémentaires de `-vvvv` ne sont pas affichées.

## Créer, modifier et rechiffrer des fichiers

`volant vault create`, `edit` et `rekey` travaillent sur les mêmes fichiers qu'`ansible-vault`.

- `create` prend un seul nom de fichier. Il refuse avec le code de sortie 5 sauf si la sortie standard est un terminal ou si `--skip-tty-check` est donné, et avec le code de sortie 1 quand le fichier existe. Un répertoire manquant est créé, avec un avertissement. L'éditeur peut laisser le fichier vide : le résultat est un fichier vault valide qui contient un texte vide.
- `edit` déchiffre chaque fichier, ouvre l'éditeur et chiffre le résultat avec le secret qui a ouvert le fichier et le label qu'il avait. Quand le texte n'a pas changé, le fichier n'est pas réécrit. Un fichier modifié garde son mode.
- L'éditeur est `EDITOR`, sinon `ANSIBLE_EDITOR`, sinon `[defaults] editor`, sinon `vi`, découpé en mots comme le ferait un shell, avec le nom du fichier ajouté. Son statut de sortie n'est pas regardé, comme dans ansible-vault.
- L'éditeur travaille sur une copie en mode `0600` dans un répertoire privé sous `ANSIBLE_LOCAL_TEMP` (`[defaults] local_tmp`, sinon `~/.ansible/tmp`). La copie est écrasée puis supprimée à chaque sortie, y compris avec un éditeur en échec et avec SIGTERM ou SIGHUP. Ctrl-C va à l'éditeur.
- `rekey` déchiffre chaque fichier avec les anciens secrets et le réécrit avec le nouveau, en gardant son mode. Le nouveau secret vient de `--new-vault-password-file`, de `--new-vault-id LABEL@SOURCE`, ou des invites `New Vault password: ` et `Confirm New Vault password: `. Il affiche `Rekey successful` sur l'erreur standard, quelle que soit la sortie standard.

## Chiffrer des chaînes

`volant vault encrypt_string` transforme une valeur en bloc `!vault |` pour un fichier de variables. Chaque ligne du texte chiffré est indentée de dix espaces, et chaque bloc se termine par un saut de ligne :

```yaml
mykey: !vault |
          $ANSIBLE_VAULT;1.1;AES256
          66313561363937653234366564623639...
```

- `-n NAME` nomme la chaîne placée au même rang et `--stdin-name NAME` nomme l'entrée standard. Sans nom, le bloc commence par `!vault |`. `--vault-id LABEL@SOURCE` donne un en-tête `1.2` avec le label.
- Sans chaîne, ou avec `-`, le texte est lu sur l'entrée standard jusqu'à la fin. Une entrée vide sort avec le code 5.
- Avec plus d'une chaîne, l'erreur standard porte une ligne de commentaire par chaîne, et une note quand il y a moins de noms que de chaînes. La sortie standard ne contient que les blocs.
- `-p` demande un nom de variable, puis la chaîne, qui n'est pas affichée à la frappe sauf si `--show-input` est donné. Il ne peut pas être combiné avec la lecture de l'entrée standard.
- Sur un terminal, `Encryption successful` va sur l'erreur standard avant les blocs.
- `--output FILE` écrit les blocs dans le fichier, en mode `0600`.

## Limites connues

- Les arguments `salt` et `wrap_object` du filtre `vault` sont refusés comme non encore pris en charge.
- Une valeur `!vault` écrite dans les propres `vars:` d'un play n'affiche aucune ligne `Origin:` quand aucun secret ne l'ouvre.
- Un `set_fact` d'un simple `{{ x }}` qui contient une valeur chiffrée perd l'étiquette vault : `is vault_encrypted` répond donc faux sur le fact là où ansible-core répond vrai.
- À `-vvvvv`, `vault view`, `decrypt` et `rekey` n'affichent pas la trace d'ansible-core pour chaque secret essayé (`Trying secret FileVaultSecret(...) for vault_id=...`). Les secrets sont essayés dans le même ordre.

La [page de la ligne de commande](/fr/docs/reference/cli/#vault) donne les options.
