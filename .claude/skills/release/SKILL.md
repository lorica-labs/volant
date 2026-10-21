---
name: release
description: Cut a Volant release through the release-plz pull request and cargo-dist, then verify the published artifacts.
---

# Releasing Volant

release-plz and cargo-dist do the work. Cutting a release means merging one pull request, then checking that what came out is what you expected.

1. Check the open release pull request (label `release`, opened by release-plz). Read `CHANGELOG.md` in the diff: entries are user-facing, grouped under Added, Changed, Fixed, Removed, Security. Edit wording in the pull request if needed.
2. Confirm the version follows SemVer for the changes listed. Pre-releases use `0.1.0-alpha.1`, `0.1.0-rc.1` forms.
3. Merge with `gh pr merge --squash --delete-branch`. release-plz tags `vX.Y.Z` and publishes the crates.
4. Watch the release workflow: `gh run list --workflow release.yml --limit 1` then `gh run watch <id>`.
5. Verify the release page has one archive per target (six controller archives and four agent archives, see the table in `docs/src/content/docs/start/installation.mdx`), `sha256.sum`, the shell installer and provenance attestations: `gh release view vX.Y.Z`. Then check the libraries of the published files. The release workflow ran this gate before it published, so a failure here means the published bytes differ from the checked ones:
   ```sh
   d="$(mktemp -d)" && gh release download vX.Y.Z -p 'volant-*.tar.xz' -D "$d" && just link-check "$d"/volant-*.tar.xz
   ```
   Run it on Linux for the Linux archives. The macOS archives need `otool`, so check them on a Mac.
6. Verify installation from a clean shell: `cargo binstall volant@X.Y.Z` then `volant --version`.
   Then, on a Linux x86_64 machine, `just smoke vX.Y.Z` and `just smoke vX.Y.Z x86_64-unknown-linux-gnu`: each runs one task with the published controller alone, which proves the agents it embeds are there. Last, run the static controller in a `FROM scratch` container:
   ```sh
   d="$(mktemp -d)" && gh release download vX.Y.Z -p volant-x86_64-unknown-linux-musl.tar.xz -D "$d" && tar -xJf "$d"/volant-x86_64-unknown-linux-musl.tar.xz -C "$d" --strip-components=1 && just scratch-test "$d/volant"
   ```
7. Announce in Discussions under Announcements with the changelog section.

The `custom-release-link-check` job of the workflow checks every archive before `host` publishes: when it fails, nothing is published. If step 4 fails, fix on a branch, open a pull request, and re-run the workflow from the tag after the fix is merged. Never move a tag.
