# Introduction

Volant is a fast, drop-in engine for Ansible playbooks. It runs your playbooks, roles and inventories as they are, over SSH to Linux hosts. It uploads a small agent once per host and sends tasks to it in batches.

The project is in pre-alpha, and so is this site. [Playbooks](playbooks.md) covers how a play is compiled and what runs when it does. [Keywords](keywords.md) and [Modules](modules.md) say what this release executes and what it refuses before it connects anywhere; a collection is among the things it refuses, so a playbook that names one does not run yet.
