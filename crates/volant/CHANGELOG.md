# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.3](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.2...v0.1.0-alpha.3) - 2026-09-07

### Added

- *(controller)* add extra vars, limit and ansible.cfg defaults ([#69](https://github.com/lorica-labs/volant/pull/69))
- *(controller)* render per host and batch tasks up to boundaries ([#66](https://github.com/lorica-labs/volant/pull/66))
- *(controller)* parse when, loop, register, vars and conditions ([#65](https://github.com/lorica-labs/volant/pull/65))
- *(controller)* add ansible filters, tests and lookups to the templar ([#64](https://github.com/lorica-labs/volant/pull/64))
- *(controller)* render jinja2 templates with strict undefined vars ([#62](https://github.com/lorica-labs/volant/pull/62))
- *(controller)* layer variable sources with ansible precedence ([#61](https://github.com/lorica-labs/volant/pull/61))
- *(agent)* kill process groups, add task timeouts and signal codes ([#54](https://github.com/lorica-labs/volant/pull/54))

### Fixed

- *(controller)* variables, conditions and per-playbook paths ([#72](https://github.com/lorica-labs/volant/pull/72))
- *(controller)* stop a deferred render error reviving a failed host ([#68](https://github.com/lorica-labs/volant/pull/68))
- *(controller)* resolve YAML 1.1 scalar spellings like PyYAML ([#63](https://github.com/lorica-labs/volant/pull/63))
- *(controller)* stop the agent's task when the link is dropped ([#57](https://github.com/lorica-labs/volant/pull/57))
- *(controller)* cancel batches on interrupt and bound the handshake ([#56](https://github.com/lorica-labs/volant/pull/56))

### Other

- drop a private path from a public source comment ([#73](https://github.com/lorica-labs/volant/pull/73))
- count banner characters, share frame codec, tighten visibility ([#70](https://github.com/lorica-labs/volant/pull/70))
- *(controller)* switch yaml to saphyr and detect vault tags ([#60](https://github.com/lorica-labs/volant/pull/60))
- share the native module registry between controller and agent ([#58](https://github.com/lorica-labs/volant/pull/58))
- enforce the commit subject length and tidy release tooling ([#52](https://github.com/lorica-labs/volant/pull/52))

## [0.1.0-alpha.2](https://github.com/lorica-labs/volant/compare/v0.1.0-alpha.1...v0.1.0-alpha.2) - 2026-09-06

### Added

- *(controller)* run playbooks with the linear strategy over local connections ([#47](https://github.com/lorica-labs/volant/pull/47))
- *(controller)* render results and the play recap like ansible-playbook ([#45](https://github.com/lorica-labs/volant/pull/45))
- *(controller)* locate the agent and run it through a local transport ([#43](https://github.com/lorica-labs/volant/pull/43))
- *(controller)* load playbooks with the supported play and task keywords ([#42](https://github.com/lorica-labs/volant/pull/42))
- *(controller)* read static ini inventories and resolve host patterns ([#40](https://github.com/lorica-labs/volant/pull/40))

### Fixed

- *(agent)* mark a skipped command as skipped ([#49](https://github.com/lorica-labs/volant/pull/49))
- *(controller)* match ansible banners and never lose a dead host ([#48](https://github.com/lorica-labs/volant/pull/48))
- *(render)* derive the recap host column from a shared constant ([#46](https://github.com/lorica-labs/volant/pull/46))
- *(controller)* report a truncated frame and survive a cancelled recv ([#44](https://github.com/lorica-labs/volant/pull/44))
- *(controller)* terminate cycles and reject malformed inventory lines ([#41](https://github.com/lorica-labs/volant/pull/41))

### Other

- add the project mark to the readme and the documentation site ([#29](https://github.com/lorica-labs/volant/pull/29))
