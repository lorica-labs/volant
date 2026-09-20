# Introduction

Volant is a fast engine for Ansible playbooks. It reads your playbooks, roles and inventories as they are and drives Linux hosts over SSH, from Linux or macOS. It uploads a small agent once per host and keeps it for the whole run, so no task starts a Python interpreter. It executes less than it reads, and the pages below say where the line falls.

The project is in pre-alpha, and so is this site. [Playbooks](playbooks.md) covers how a play is compiled and what runs when it does. [Keywords](keywords.md) and [Modules](modules.md) say what this release executes and what it refuses before it connects anywhere; a collection is among the things it refuses, so a playbook that names one does not run yet.
