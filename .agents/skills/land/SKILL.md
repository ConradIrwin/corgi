---
name: land
description: >-
  Land changes by merging to main. Only invoke when the user requests landing,
  not for review, preparation, or skill installation.
metadata:
  delta-action: land
---

When the user requests landing, commit any remaining thread changes and push
the current commit directly to `main` on the GitHub remote.
Preserve unrelated work and leave `README.md` unchanged.

If remote `main` has advanced, merge it first. Resolve conflicts automatically,
asking only when a product decision is needed. Never force-push.

If fmt, tests, or clippy haven't already passed locally for the current code,
run the missing checks once before pushing. Reuse existing successful results
when nothing relevant has changed; do not push with failed checks.

`Cargo.toml` and `Cargo.lock` carry the checked-in version. Landing an ordinary
commit does not release Corgi: `.github/workflows/test.yml` runs on every branch
push, including `main`, to populate the debug build and test cache. Never
invent a per-push version or modify the manifest in CI.

If a change bumps the pinned Zig or Metal toolchain, its release assets must
exist before branch tests and landing. Have a maintainer dispatch
`.github/workflows/prepare-toolchain-assets.yml` on the reviewed branch, then
rerun tests. Normal CI only consumes these assets; do not give its test job
GitHub release write access. The tag workflow still reads the published
digests to pin the release binary.

If the changes being landed explicitly bump the package version, verify the
lockfile agrees. After the landed commit's `main` tests pass, create and push
the matching `v<version>` tag on that commit. Check the GitHub remote, not just
local tags: if the tag exists remotely, require it to point at this commit;
if only a matching local tag exists, push it. Otherwise create and push it.
That tag starts `.github/workflows/publish.yml`, which checks the tag against
the manifest, builds the release asset, and publishes to crates.io. Never
retarget an existing tag or force-push. A landing request authorizes this
release for an intentional version bump; do not ask for permission again.
Without a version bump, do not create a release tag.

Verify the commit reached `main` and monitor its test CI. If releasing,
monitor the tag's publish workflow too and report when the new Corgi build is
released. If CI fails after pushing, say the code landed but CI or the release
failed. In a subthread, report success only after verified landing and, when
applicable, release; report blockers as failures with verified commit and CI
links.
