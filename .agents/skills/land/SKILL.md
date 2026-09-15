---
name: land
description: >-
  Land changes by merging to main. Only invoke when the user requests landing,
  not for review, preparation, or skill installation.
metadata:
  delta-action: land
---

When the user requests landing, commit any remaining thread changes and push
the current commit directly to `main` on the GitHub remote, never `local`.
The request authorizes the automatic release; do not ask for permission again.
Preserve unrelated work and leave `README.md` unchanged.

If remote `main` has advanced, merge it first. Resolve conflicts automatically,
asking only when a product decision is needed. Never force-push.

If fmt, tests, or clippy haven't already passed locally for the current code,
run the missing checks once before pushing. Reuse existing successful results
when nothing relevant has changed; do not push with failed checks.

Verify the commit reached `main`. Monitor its CI for failures and tell the user
when the new Corgi build is released (`.github/workflows/publish.yml`).
In a subthread, use `report_subthread_status`: success only after verified
landing and release, failure for blockers. Use a short title and one-line
description with verified commit and CI links. If CI fails after pushing,
say the code landed but the release failed.
