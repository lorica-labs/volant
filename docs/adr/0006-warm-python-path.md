# Use one union blob per warm Python run

- Status: accepted
- Date: 2026-09-20

## Context

The Python path must run ansible-core modules on a managed host without installing ansible-core there. A process per task costs 340 ms. Forking from a Python server that has loaded nothing costs 266 ms, which is not enough to justify the machinery.

Loading `module_utils` from one module's zip and then forking costs 12.8 ms, but it pins Python's `ansible` package to that one zip. Every other module then fails to import. This was measured, and it fails loudly rather than silently.

The entries shared between different module zips are byte-identical: zero conflicts across 146 entries from ten modules. Merging them into one zip is well defined, and the builder treats a conflict as a hard error.

## Decision

Build one union blob per run, not one blob per module. The blob contains every module and its imported `module_utils`, is identified by the BLAKE3 hash of its contents, and is cached on the managed host. The Python server loads it once and forks a child for each task.

The union blob for the five golden modules is exactly 631050 bytes, compared with 2307982 bytes for separate zips. That is a 72.7 percent saving. The preloaded fork path costs 12.8 ms per task, and loading the blob costs 264.5 ms once per server.

## Consequences

- A run gets one consistent `module_utils` set, so modules do not import from incompatible zips.
- The builder must stop on a conflicting shared entry rather than choose one version.
- The managed host caches and verifies the blob before using it.
- The controller needs ansible-core; the managed host does not.
