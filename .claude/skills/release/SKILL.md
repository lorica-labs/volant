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
5. Verify the release page has one archive per target, `sha256.sum`, the shell installer and provenance attestations: `gh release view vX.Y.Z`.
6. Verify installation from a clean shell: `cargo binstall volant@X.Y.Z` then `volant --version`.
7. Announce in Discussions under Announcements with the changelog section.

If step 4 fails, fix on a branch, open a pull request, and re-run the workflow from the tag after the fix is merged. Never move a tag.
