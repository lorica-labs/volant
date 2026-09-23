# Getting help

- Start with the [documentation](https://volant.sh/), in particular [Will my playbook run?](https://volant.sh/start/compatibility/).
- Questions and ideas: [GitHub Discussions](https://github.com/lorica-labs/volant/discussions).
- Bugs and feature requests: [GitHub Issues](https://github.com/lorica-labs/volant/issues), using the templates. A playbook that behaves differently under Volant and `ansible-playbook` has its own compatibility template.
- Security problems: see [SECURITY.md](SECURITY.md).

When you report a problem, include:

- your Volant version (`volant --version`);
- the ansible-core version on the controller, if the playbook uses Python modules or gathers facts;
- a minimal playbook and inventory that show the problem;
- the full output, with `-vvv` if you can.
