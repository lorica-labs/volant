# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
