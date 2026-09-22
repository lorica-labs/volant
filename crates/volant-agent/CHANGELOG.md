# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.7](https://github.com/lorica-labs/volant/compare/volant-agent-v0.1.0-alpha.6...volant-agent-v0.1.0-alpha.7) - 2026-09-22

### Added

- *(modules)* run assert, fail and pause on the controller ([#179](https://github.com/lorica-labs/volant/pull/179))
- *(agent)* run python modules in a warm forking server ([#168](https://github.com/lorica-labs/volant/pull/168))
- *(agent)* report the host's python interpreters at handshake ([#161](https://github.com/lorica-labs/volant/pull/161))
- *(protocol)* address module payloads by content hash ([#159](https://github.com/lorica-labs/volant/pull/159))

### Fixed

- match the reference on facts, guarded commands and registered results ([#181](https://github.com/lorica-labs/volant/pull/181))
- *(agent)* fail a python task that returns no result ([#172](https://github.com/lorica-labs/volant/pull/172))

### Other

- describe the warm python path and record the decision ([#169](https://github.com/lorica-labs/volant/pull/169))

## [0.1.0-alpha.6](https://github.com/lorica-labs/volant/compare/volant-agent-v0.1.0-alpha.5...volant-agent-v0.1.0-alpha.6) - 2026-09-20

### Added

- *(modules)* declare which arguments each native module honours ([#145](https://github.com/lorica-labs/volant/pull/145))

### Fixed

- *(preflight)* check what a dynamic include splices in ([#155](https://github.com/lorica-labs/volant/pull/155))
- *(command)* hold the deadline over the stdin write ([#154](https://github.com/lorica-labs/volant/pull/154))
- *(command)* keep the deadline over the child and its streams ([#144](https://github.com/lorica-labs/volant/pull/144))
- *(executor)* show every event a task produces through the renderer ([#143](https://github.com/lorica-labs/volant/pull/143))
- *(command)* resolve creates and removes where the command runs ([#142](https://github.com/lorica-labs/volant/pull/142))

### Other

- send the ssh quickstart to the host it names ([#152](https://github.com/lorica-labs/volant/pull/152))
- say what this release expands and when links close ([#150](https://github.com/lorica-labs/volant/pull/150))
- say what this release runs and what it only partly answers ([#148](https://github.com/lorica-labs/volant/pull/148))
- ship the Linux agents inside every controller archive ([#147](https://github.com/lorica-labs/volant/pull/147))
- *(executor)* say why in one line and move the rest out ([#133](https://github.com/lorica-labs/volant/pull/133))
- declare workspace lints and name each allowance ([#123](https://github.com/lorica-labs/volant/pull/123))

## [0.1.0-alpha.5](https://github.com/lorica-labs/volant/compare/volant-agent-v0.1.0-alpha.4...volant-agent-v0.1.0-alpha.5) - 2026-09-16

### Added

- *(controller)* retry with until, censor no_log, pass environment ([#107](https://github.com/lorica-labs/volant/pull/107))

### Other

- describe play compilation, roles, blocks, handlers and tags ([#117](https://github.com/lorica-labs/volant/pull/117))

## [0.1.0-alpha.4](https://github.com/lorica-labs/volant/compare/volant-agent-v0.1.0-alpha.3...volant-agent-v0.1.0-alpha.4) - 2026-09-09

### Other

- describe connections, the agent cache and privilege escalation ([#89](https://github.com/lorica-labs/volant/pull/89))

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
