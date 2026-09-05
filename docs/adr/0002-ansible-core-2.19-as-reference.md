# Use ansible-core 2.19 as the compatibility reference

- Status: accepted
- Date: 2026-09-05

## Context

Volant promises to run existing playbooks unchanged. "Unchanged" needs a definition: ansible-core changes behaviour between versions, and 2.19 rewrote templating (data tagging) with some intentional breaks.

## Decision

Volant reproduces the behaviour of ansible-core 2.19. Differences from 2.19 are bugs. Golden tests and the compatibility harness compare against that version. A nightly job also runs against ansible-core's development branch to see changes coming.

## Consequences

- Playbooks that only worked on older cores because of behaviour removed in 2.19 are out of scope.
- Moving the reference to a newer core is a new decision, recorded here, with a major or minor version bump depending on what changes.
