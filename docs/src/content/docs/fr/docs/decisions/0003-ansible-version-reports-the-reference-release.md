---
sourceHash: d52d376402f0
title: "`ansible_version` rapporte la version de référence d'ansible-core"
description: "Fiche de décision 0003."
sidebar:
  label: "0003 · ansible_version"
  order: 3
---

| Statut | Date |
|---|---|
| accepté | 2026-09-06 |

## Contexte

De nombreux rôles Galaxy se branchent sur `ansible_version.full` ou `ansible_version.major` pour choisir un chemin de code. Volant n'est pas Ansible, mais il reproduit le comportement d'une version d'ansible-core (voir la [fiche de décision 0002](/fr/docs/decisions/0002-ansible-core-2-19-as-reference/)). Laisser `ansible_version` indéfinie ferait échouer ces rôles sur une variable non définie ; inventer une valeur propre à Volant les enverrait sur des chemins non testés.

## Décision

`ansible_version` contient la version de référence que Volant reproduit, sous la forme de dictionnaire que les rôles attendent (`full`, `major`, `minor`, `revision`, `string`). Une variable distincte, `volant_version`, contient la version de Volant elle-même.

## Conséquences

- Les rôles écrits pour la version de référence se comportent comme sous celle-ci.
- Quand la référence change (procédure de la [fiche de décision 0002](/fr/docs/decisions/0002-ansible-core-2-19-as-reference/)), `ansible_version` change avec elle, dans la même modification.
- Un playbook qui doit distinguer Volant d'Ansible teste `volant_version is defined`.
