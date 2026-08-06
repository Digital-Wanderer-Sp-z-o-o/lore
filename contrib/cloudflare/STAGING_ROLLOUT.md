<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Cloudflare and Hetzner staging rollout

This runbook rolls out one reviewed Lore commit to the private Cloudflare
backend and the single-node Hetzner staging server. It deliberately separates
upload, canary verification, promotion, server replacement, and rollback.
Never use it for production data or claim high availability from this
single-node profile.

## Non-negotiable gates

- Work from a clean checkout whose `HEAD` is the reviewed, pushed commit.
- Record the active Worker deployment and current server image before changing
  either service. Never infer them from a local branch name.
- The Durable Object `exports` map must not delete, rename, transfer, or change
  the storage type of any existing class. Such lifecycle changes can make a
  normal Worker rollback impossible.
- Schema changes must be additive and old server/Worker code must tolerate the
  added tables or columns. The lock schema in this rollout explicitly migrates
  `1 -> 2` and preserves existing rows.
- Promote the Worker to 100% before starting a server that requires its new
  API. Do not leave old and new Worker versions split while the new server is
  active.
- The Worker HMAC secret must exist on both sides, but must never be printed,
  copied into command history, or stored in an image.
- Stop at the first health, signature, authorization, repository verification,
  false-not-found, or data-integrity failure.

## 1. Capture the immutable baseline

From `contrib/cloudflare/lore-do-backend` in the clean checkout:

```powershell
$rollout = New-Item -ItemType Directory -Force `
  (Join-Path $env:TEMP ("lore-rollout-" + (Get-Date -Format yyyyMMdd-HHmmss)))
git rev-parse HEAD | Set-Content (Join-Path $rollout target-git-sha.txt)
npx wrangler deployments list --env staging --json |
  Set-Content (Join-Path $rollout worker-deployments-before.json)
npx wrangler versions list --env staging --json |
  Set-Content (Join-Path $rollout worker-versions-before.json)
npx wrangler secret list --env staging --format json |
  Set-Content (Join-Path $rollout worker-secrets-before.json)
npx wrangler r2 bucket info archigma-lore-staging |
  Set-Content (Join-Path $rollout r2-before.txt)
Invoke-RestMethod `
  https://archigma-lore-do-staging.damian-podwiazka.workers.dev/health |
  ConvertTo-Json -Depth 8 |
  Set-Content (Join-Path $rollout worker-health-before.json)
```

Accept exactly one active Worker version at 100% and one required secret named
`AUTH_SECRET_ACCESS_KEY`. Record that version ID as `$oldWorkerVersion`. The
R2 bucket must remain private and named `archigma-lore-staging`.

On Hetzner, before changing the checkout or container:

```bash
set -euo pipefail
current_image_id="$(docker inspect archigma-lore-staging --format '{{.Image}}')"
current_started_at="$(docker inspect archigma-lore-staging --format '{{.State.StartedAt}}')"
current_restarts="$(docker inspect archigma-lore-staging --format '{{.RestartCount}}')"
rollback_tag="rollback-$(date -u +%Y%m%d-%H%M%S)"
docker image tag "$current_image_id" "archigma-lore-server:$rollback_tag"
printf '%s\n' "$current_image_id" > /var/lib/lore/rollout-current-image.txt
printf '%s\n' "$rollback_tag" > /var/lib/lore/rollout-rollback-tag.txt
printf '%s %s\n' "$current_started_at" "$current_restarts" \
  > /var/lib/lore/rollout-container-before.txt
curl --fail --silent --show-error http://127.0.0.1:41339/health_check
```

Tagging the current image is required because the original Compose profile did
not give historical builds immutable Git-derived tags.

## 2. Validate without changing Cloudflare

```powershell
git status --porcelain
git rev-parse HEAD
git rev-parse '@{upstream}'
npm ci
npm run preflight:staging
```

`git status --porcelain` must be empty and the two revisions must match. The
preflight runs generated-binding checks, TypeScript, Worker-runtime tests, and
`wrangler versions upload --dry-run --strict`. Review Wrangler's Durable Object
reconciliation output; this rollout permits no class lifecycle action.

## 3. Upload a zero-traffic Worker canary

Uploading and deploying are separate operations:

```powershell
$targetSha = git rev-parse HEAD
npm run upload:staging -- `
  --tag "git-$targetSha" `
  --message "Lore staging canary $targetSha"
npx wrangler versions list --env staging --json
```

Record the uploaded version ID as `$newWorkerVersion`, then add it to the
deployment at zero traffic while retaining the captured old version at 100%:

```powershell
npx wrangler versions deploy `
  "$newWorkerVersion@0%" "$oldWorkerVersion@100%" `
  --env staging --message "Lore zero-traffic canary $targetSha" --yes
```

Do not continue unless `wrangler deployments list --env staging --json` shows
exactly those two IDs and percentages.

## 4. Prove the exact Worker canary

Run the signed smoke from the repository root on a trusted machine that already
has the shared secret, normally the Hetzner staging host. Use a dedicated empty
canary repository ID and address so the compatibility reads do not touch a
user's locks or metadata.

```bash
set -a
. ./contrib/hetzner/.env
set +a
export LORE_WORKER_BASE_URL='https://archigma-lore-do-staging.damian-podwiazka.workers.dev'
export LORE_WORKER_NAME='archigma-lore-do-staging'
export LORE_WORKER_VERSION_ID="$new_worker_version"
export LORE_SMOKE_PHASE='zero-traffic'
export LORE_SMOKE_REPOSITORY_ID="$dedicated_canary_repository_id"
export LORE_SMOKE_OBLITERATION_HASH="$dedicated_canary_obliteration_hash"
export LORE_SMOKE_OBLITERATION_CONTEXT="$dedicated_canary_obliteration_context"
npm --prefix contrib/cloudflare/lore-do-backend run smoke:staging
unset LORE_CLOUDFLARE_SHARED_SECRET
```

The smoke fails unless Cloudflare honors the version override, `/health`
returns the exact requested version ID and advertised capabilities, and the
new top-level Worker can complete HMAC-signed reads through the version-one
lock and immutable RPC contracts.

This zero-traffic check does **not** prove a new Durable Object RPC method or
schema migration. Durable Object updates are eventually consistent, and a
version-overridden Worker can still reach the previously deployed Durable
Object code. Keep all existing RPC method names, signatures, and HTTP request
shapes compatible in both directions during a rolling deployment; add new
behavior under distinct method names.

## 5. Promote or abandon the Worker

After a successful smoke, promote only the new version:

```powershell
npx wrangler versions deploy "$newWorkerVersion@100%" `
  --env staging --message "Promote Lore Worker $targetSha" --yes
npx wrangler deployments list --env staging --json
Invoke-RestMethod `
  https://archigma-lore-do-staging.damian-podwiazka.workers.dev/health |
  ConvertTo-Json -Depth 8
```

The normal health response must now expose `$newWorkerVersion`. If the canary
health succeeds, run the post-promotion smoke from the trusted host. It sends
normal traffic without a version override and is the first gate that may call
the new audit RPCs and activate the additive `1 -> 2` schema migration:

```bash
export LORE_SMOKE_PHASE='post-promotion'
for attempt in $(seq 1 12); do
  npm --prefix contrib/cloudflare/lore-do-backend run smoke:staging && break
  test "$attempt" -lt 12 || exit 1
  sleep 5
done
unset LORE_CLOUDFLARE_SHARED_SECRET
```

The bounded retry allows Durable Object code propagation to converge. Do not
start the new Lore Server unless both signed audit queries succeed. If the
zero-traffic check or post-promotion gate fails before the new server is
active, abandon the Worker explicitly:

```powershell
npx wrangler versions deploy "$oldWorkerVersion@100%" `
  --env staging --message "Abandon Lore Worker canary" --yes
```

This explicit version deployment is preferred over an implicit `wrangler
rollback`. It is valid only because this rollout leaves the Durable Object
class lifecycle and storage types unchanged. The additive `lock_recovery_audit` and
`obliteration_audit` tables are backward-compatible and ignored by the old Worker. The new Worker
keeps old Durable Object RPC names and signatures, legacy lock-release and
obliteration request contracts, and additive schemas during the ordered
rolling deployment; only the new server uses audited routes.

## 6. Build and run the server canary

On Hetzner, check out the same reviewed commit and keep the tree clean:

```bash
set -euo pipefail
target_sha='<reviewed-40-character-sha>'
test "$(git rev-parse HEAD)" = "$target_sha"
test -z "$(git status --porcelain)"
cd contrib/hetzner
LORE_SERVER_TAG="$target_sha" docker compose build lore-server
test "$(docker image inspect "archigma-lore-server:$target_sha" \
  --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')" = "$target_sha"
sudo install -d -m 0750 /var/lib/lore/cache-canary
LORE_SERVER_TAG="$target_sha" docker compose -f compose.canary.yaml up -d
curl --fail --retry 20 --retry-delay 2 \
  http://127.0.0.1:42339/health_check
LORE_SERVER_TAG="$target_sha" docker compose -f compose.canary.yaml \
  logs --tail 100 lore-server-canary
```

From an authenticated client on WireGuard, prove the canary protocol endpoint
without mutating a real repository:

```bash
lore repository list lore://10.80.0.10:42337
```

Also run a clone and `lore repository verify state` against a small dedicated
canary repository. Do not use a production working copy for this gate.

Remove the canary only after its logs and verification artifacts have been
saved:

```bash
LORE_SERVER_TAG="$target_sha" docker compose -f compose.canary.yaml down
```

## 7. Promote the server and verify

```bash
set -euo pipefail
LORE_SERVER_TAG="$target_sha" docker compose up -d --no-build lore-server
curl --fail --retry 20 --retry-delay 2 \
  http://127.0.0.1:41339/health_check
test "$(docker inspect archigma-lore-staging \
  --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')" = "$target_sha"
test "$(docker inspect archigma-lore-staging --format '{{.RestartCount}}')" = 0
docker compose logs --tail 100 lore-server
```

Then run, in order: allowed-user repository list, denied-user repository list,
small canary clone and verification, self lock/release, two-owner contention,
administrator owner-CAS recovery, and paginated audit read. Any authorization
or integrity mismatch is a rollback condition.

Before enabling Desktop obliteration, verify that the new server calls only the audited
obliteration routes and that no legacy obliteration route appears in Worker logs. Run the
interruption/retry smoke against a disposable canary address, then confirm the signed audit query
shows `payload_obliterated` (or `payload_retained` for the shared-association case) with the expected
repository, actor, correlation ID, and completion timestamp.

## 8. Server rollback

The Cloudflare Worker remains on the new backward-compatible version while the
server is rolled back. This preserves its additive schema and avoids changing
two layers during incident response:

```bash
set -euo pipefail
rollback_tag="$(cat /var/lib/lore/rollout-rollback-tag.txt)"
LORE_SERVER_TAG="$rollback_tag" docker compose up -d --no-build lore-server
curl --fail --retry 20 --retry-delay 2 \
  http://127.0.0.1:41339/health_check
docker compose logs --tail 100 lore-server
```

Only after the old server is healthy may the Worker be returned to the recorded
old version. Never remove R2 objects, Durable Object namespaces, tables, or
schema-version rows as part of rollback.

## Evidence to retain

- target Git SHA, old/new Worker version IDs, deployment JSON before/after;
- Worker health and signed canary output;
- old/new server image IDs and OCI revision label;
- server health, restart count, logs, and canary repository verification;
- allowed/denied auth results, lock contention, recovery, and audit evidence;
- timestamps, operator, decision, and any rollback reason.

Copy the accepted evidence summary into the Archigma LORE Desktop pilot plan.

## Platform references

- [Cloudflare version overrides](https://developers.cloudflare.com/workers/versions-and-deployments/version-overrides/)
- [Cloudflare Durable Object update consistency](https://developers.cloudflare.com/durable-objects/platform/known-issues/)
- [Cloudflare Worker RPC compatibility](https://developers.cloudflare.com/workers/runtime-apis/rpc/)
- [Cloudflare Worker rollbacks and binding limits](https://developers.cloudflare.com/workers/versions-and-deployments/rollbacks/)
- [Worker version metadata binding](https://developers.cloudflare.com/workers/runtime-apis/bindings/version-metadata/)
- [Durable Object class exports](https://developers.cloudflare.com/durable-objects/reference/durable-objects-migrations/)
