# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.3](https://github.com/lorica-labs/volant/compare/volant-agent-v0.1.0-alpha.2...volant-agent-v0.1.0-alpha.3) - 2026-09-07

### Added

- *(agent)* kill process groups, add task timeouts and signal codes ([#54](https://github.com/lorica-labs/volant/pull/54))

### Fixed

- *(agent)* keep the trailing newline in raw output ([#59](https://github.com/lorica-labs/volant/pull/59))
- *(controller)* stop the agent's task when the link is dropped ([#57](https://github.com/lorica-labs/volant/pull/57))
- *(controller)* cancel batches on interrupt and bound the handshake ([#56](https://github.com/lorica-labs/volant/pull/56))
- *(agent)* match ansible's task timeout result shape ([#55](https://github.com/lorica-labs/volant/pull/55))

### Other

- count banner characters, share frame codec, tighten visibility ([#70](https://github.com/lorica-labs/volant/pull/70))
- share the native module registry between controller and agent ([#58](https://github.com/lorica-labs/volant/pull/58))

## [0.1.0-alpha.2](https://github.com/lorica-labs/volant/compare/volant-agent-v0.1.0-alpha.1...volant-agent-v0.1.0-alpha.2) - 2026-09-06

### Added

- *(agent)* run task batches with fail-fast and cancellation ([#38](https://github.com/lorica-labs/volant/pull/38))
- *(agent)* add native command, shell and raw modules ([#35](https://github.com/lorica-labs/volant/pull/35))
- *(agent)* answer the controller handshake over stdin and stdout frames ([#33](https://github.com/lorica-labs/volant/pull/33))

### Fixed

- *(agent)* mark a skipped command as skipped ([#49](https://github.com/lorica-labs/volant/pull/49))
- *(agent)* log a broken stdin during a batch and exit non-zero ([#39](https://github.com/lorica-labs/volant/pull/39))
- *(agent)* avoid stdin deadlock and misattributed chdir errors ([#37](https://github.com/lorica-labs/volant/pull/37))
- *(agent)* report stdin read errors and reap child on test drop ([#34](https://github.com/lorica-labs/volant/pull/34))

### Other

- add the project mark to the readme and the documentation site ([#29](https://github.com/lorica-labs/volant/pull/29))
