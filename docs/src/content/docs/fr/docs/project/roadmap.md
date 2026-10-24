---
sourceHash: 12e3021ea96d
title: Feuille de route
description: Ce vers quoi travaille Volant, version par version, et où reprendre le travail.
---

Volant est en pré-alpha. Le [tableau de la feuille de route de Volant](https://github.com/orgs/lorica-labs/projects/1) contient le plan sous forme d'issues GitHub, regroupées par version. Chaque issue dit quoi construire, comment se comporte ansible-core 2.19.12, comment vérifier le résultat, et par où commencer dans le code : n'importe qui peut donc en prendre une. Le [changelog](https://github.com/lorica-labs/volant/blob/main/CHANGELOG.md) consigne ce qui a réellement été livré.

## Versions

| Jalon | Ce qu'il apporte |
|---|---|
| [v0.1.0-alpha.11](https://github.com/lorica-labs/volant/milestone/2) | La suite de commandes : `volant adhoc`, `volant vault` et `volant inventory` en Rust ; `doc`, `config`, `galaxy`, `pull` et `console` par le runtime géré ; des noms de commande `ansible-*` facultatifs, pour que les scripts existants s'exécutent sans changement sans qu'Ansible soit installé |
| [v0.1.0-alpha.12](https://github.com/lorica-labs/volant/milestone/10) | Un basculement sûr : enregistrer de vraies exécutions d'`ansible-playbook`, les rejouer sous Volant sans toucher aux hôtes, et verrouiller la version du moteur |
| [v0.1.0-alpha.13](https://github.com/lorica-labs/volant/milestone/3) | Les connexions de conteneurs : docker, podman, kubectl, lxd et incus, chroot, buildah, aws_ssm |
| [v0.1.0-alpha.14](https://github.com/lorica-labs/volant/milestone/9) | Travail interne : fonctions d'exécution plus courtes, tables déclaratives, consommation de mémoire |
| [v0.1.0-alpha.15](https://github.com/lorica-labs/volant/milestone/8) | Les équipements réseau (network_cli, netconf, httpapi), paramiko_ssh et les autres callbacks de sortie standard |
| [v0.1.0-alpha.16](https://github.com/lorica-labs/volant/milestone/4) | `volant galaxy` en Rust : collections et rôles |
| [v0.1.0-alpha.17](https://github.com/lorica-labs/volant/milestone/5) | `volant pull` et `volant console` en Rust |
| [v0.1.0-alpha.18](https://github.com/lorica-labs/volant/milestone/6) | Un contrôleur Windows : `volant.exe` |
| [v0.1.0-alpha.19](https://github.com/lorica-labs/volant/milestone/7) | Les hôtes Windows : winrm, psrp, runas et les modules PowerShell |

[Écarts de parité](https://github.com/lorica-labs/volant/milestone/12) liste les différences avec ansible-core trouvées jusqu'ici. Elles ne sont liées à aucune version, et chacune peut être corrigée à tout moment.

## Prendre une issue

Commentez l'issue avant de commencer, pour que deux personnes ne fassent pas le même travail. `size/S` représente environ un jour, `size/M` quelques jours, `size/L` une semaine ou plus, et `good first issue` marque un début en douceur. [CONTRIBUTING.md](https://github.com/lorica-labs/volant/blob/main/CONTRIBUTING.md) couvre la mise en place et les vérifications.

Il manque quelque chose qui vous bloque ? [Ouvrez une demande de fonctionnalité](https://github.com/lorica-labs/volant/issues/new?template=feature.yml) ou lancez un fil dans les [Discussions](https://github.com/lorica-labs/volant/discussions).
