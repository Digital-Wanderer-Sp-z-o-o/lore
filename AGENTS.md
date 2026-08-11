# LORE agent workflow

## Pull-request-only integration (hard gate)

- One task owns one isolated worktree and one `codex/...` branch. Do not edit
  another agent's branch or absorb unrelated dirty files.
- Never push directly to `main`, merge a feature branch locally into `main`, or
  bypass branch protection/checks with an administrator override.
- Commit the scoped change, push only its task branch, and open a GitHub pull
  request to the appropriate reviewed base. Use stacked PRs only when a change
  genuinely depends on another open branch; retarget to `main` after the base
  PR merges.
- Verify GitHub registered the repository workflows and created DCO, Lint, and
  PR Validate runs for the exact PR head SHA. This repository is a fork, so the
  presence of `.github/workflows/*.yml` alone is not proof that Actions are
  enabled. An empty workflow registry or zero PR runs is a blocker.
- Wait for every intended check to pass before merging through GitHub. A green
  subset, skipped required job, missing check, or open check is not acceptance.
- Inspect the actual job list and matrix fan-out. Compare it with the changed
  scope, investigate unexpected broad builds, and cancel duplicate dispatches.
  Do not launch another run while an equivalent one is queued or active.
- After the GitHub merge, fetch `origin/main` and confirm the merge commit is
  reachable from it. Report the merged SHA.

## Deployment boundary

- A code-change request, approved PR, or green CI does not authorize staging or
  production deployment. Shared-environment mutation requires an explicit
  deployment request in the current task.
- Deploy only an exact reviewed SHA reachable from remote `main`; never deploy
  an open PR, feature branch, dirty worktree, locally produced merge, or
  unlabelled image.
- Before dispatching, list active runs for the same environment and workflow.
  Keep one coordinated rollout and verify image revision, health, logs, and
  rollback evidence afterward.
- Preserve repository payloads, Durable Object state, R2 objects, and rollback
  images during rollout. Never turn cleanup or resource recreation into an
  implicit migration.
