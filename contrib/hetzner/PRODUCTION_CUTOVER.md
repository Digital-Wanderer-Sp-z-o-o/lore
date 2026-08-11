<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Archigma LORE production cutover

This runbook promotes the existing single-node Hetzner LORE service to the
canonical `lore.archigma.com` production endpoint without moving repository
payloads or revision metadata. `lore.rendermoon.com` remains a temporary
compatibility alias only.

The current Cloudflare Worker, Durable Object namespaces, and R2 bucket are
promoted **in place**. Their legacy resource names contain `staging`, but they
become production-exclusive at cutover. Do not recreate, rename, copy, empty,
or retarget those resources. Provision new isolated resources before staging is
used again.

This profile is a deliberately accepted single-host production baseline, not a
high-availability claim. Keep the previous container and image available until
post-cutover verification is complete.

## Hard gates

- Work only from a clean LORE commit that is merged into remote `main` through
  a green GitHub pull request.
- Record the target 40-character SHA and prove it is reachable from
  `origin/main`; never deploy a branch tip.
- Verify `https://archigma.com/api/v1/health` and
  `https://archigma.com/api/v1/lore/auth/jwks.json` before touching Hetzner.
- The JWKS must expose the expected production RS256 key and LORE's audience
  list must include `lore.archigma.com`, `lore.rendermoon.com`, and
  `lore-service`.
- Create a Neon production recovery branch before repository registration.
- Map users from staging to production by normalized email. Never copy a
  staging user UUID or write Better Auth organization tables directly.
- Register every existing LORE repository and its owners through the Archigma
  production API before the server starts using production authentication.
- Stop immediately on an authorization, false-not-found, clone, verification,
  lock, audit, certificate, or data-integrity failure.

## 1. Capture the live baseline

On Hetzner, record the current image, config, start time, restart count, and
health before changing the checkout:

```bash
set -euo pipefail
rollout_root="/var/lib/lore/production-cutover-$(date -u +%Y%m%d-%H%M%S)"
sudo install -d -m 0750 "$rollout_root"

docker inspect archigma-lore-staging > "$rollout_root/container-before.json"
docker inspect archigma-lore-staging \
  --format '{{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
  > "$rollout_root/container-before.txt"
docker inspect archigma-lore-staging --format '{{.Image}}' \
  > "$rollout_root/image-before.txt"
sudo cp contrib/hetzner/config.toml "$rollout_root/config-before.toml"
curl --fail --silent --show-error \
  http://127.0.0.1:41339/health_check > "$rollout_root/health-before.json"
```

Also retain the current Worker deployment/version JSON, Worker health, R2
bucket metadata, and a read-only authenticated repository list. That list must
contain the expected IDs before and after cutover.

## 2. Register production identity and repositories

Use an organization-bound Archigma production API key for an organization
owner/admin, or complete the same flow in the Archigma UI. Add the production
user to the existing organization through the product flow, then register each
existing repository under its unchanged `urc-...` ID and grant the mapped
production user the intended repository role.

Verify from the production API that:

- both existing repository IDs and names are present;
- the owner/member UUIDs are production UUIDs resolved by email;
- an unauthenticated request returns `401`;
- an allowed user can list the repositories;
- a same-organization user without a repository grant cannot discover them.

Do not proceed while the production registry is empty or still refers to a
staging user UUID.

## 3. Build the exact merged image

```bash
set -euo pipefail
target_sha='<merged-main-40-character-sha>'
git fetch origin main
git checkout --detach "$target_sha"
test -z "$(git status --porcelain)"
git merge-base --is-ancestor "$target_sha" origin/main

cd contrib/hetzner
LORE_SERVER_TAG="$target_sha" \
  docker compose -f compose.production.yaml build lore-server
test "$(docker image inspect "archigma-lore-server:$target_sha" \
  --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')" = \
  "$target_sha"
```

Tagging and verifying the OCI revision is mandatory. Do not reuse `latest`,
`staging`, or an unlabelled local image.

## 4. Run the production-auth canary

The canary shares only the durable data plane. It uses a separate disposable
local cache and alternate ports, so it can run beside the live container.

```bash
set -euo pipefail
sudo install -d -m 0750 /var/lib/lore/production-cache-canary
LORE_SERVER_TAG="$target_sha" \
  docker compose -f compose.production-canary.yaml up -d
curl --fail --retry 20 --retry-delay 2 \
  http://127.0.0.1:42339/health_check
LORE_SERVER_TAG="$target_sha" \
  docker compose -f compose.production-canary.yaml \
  logs --tail 150 lore-server-canary
```

Using a production-authenticated canary client, list the existing repositories,
clone a small disposable canary repository, verify it, push one canary revision,
verify it again, acquire/release a lock, and read the audit page. Do not mutate
an existing project merely to test the cutover. Keep the evidence in
`$rollout_root`.

Remove the canary only after all evidence is saved:

```bash
LORE_SERVER_TAG="$target_sha" \
  docker compose -f compose.production-canary.yaml down
```

## 5. Switch public port 443

The two host-network containers cannot own port 443 together. Keep the
interruption bounded and make rollback a direct container start:

```bash
set -euo pipefail
docker stop --time 30 archigma-lore-staging
LORE_SERVER_TAG="$target_sha" \
  docker compose -f compose.production.yaml up -d --no-build lore-server
curl --fail --retry 20 --retry-delay 2 \
  http://127.0.0.1:41339/health_check
test "$(docker inspect archigma-lore-production \
  --format '{{index .Config.Labels "org.opencontainers.image.revision"}}')" = \
  "$target_sha"
test "$(docker inspect archigma-lore-production \
  --format '{{.RestartCount}}')" = 0
```

From an external client, verify TLS hostname validation and authenticated LORE
operations through `lores://lore.archigma.com:443`. Repeat a read-only list and
clone/verify through `lores://lore.rendermoon.com:443` to prove the legacy alias
still works, but publish only the Archigma URL to new clients.

Run the allowed-user list, denied-user list, disposable clone/verify/push,
lock contention, admin owner-CAS recovery, paginated audit read, and repository
history checks. Confirm both existing repository IDs remain unchanged and
inspect server/Worker logs for authorization or storage errors.

After acceptance, install the production certificate reload hook:

```bash
sudo install -m 0755 reload-after-cert-renewal-production.sh \
  /etc/letsencrypt/renewal-hooks/deploy/reload-lore-production
sudo certbot renew --dry-run
```

## Rollback

Rollback the server without changing Cloudflare data or deleting cache/state:

```bash
set -euo pipefail
LORE_SERVER_TAG="$target_sha" \
  docker compose -f compose.production.yaml down
docker start archigma-lore-staging
curl --fail --retry 20 --retry-delay 2 \
  http://127.0.0.1:41339/health_check
```

Verify the legacy authenticated repository list and clone after rollback. Do
not roll back by deleting R2 objects, Durable Object state, repository records,
Neon rows, or the production recovery branch.

## Evidence to retain

- merged target SHA, PR URL, and green required checks;
- Neon recovery branch ID and production migration status;
- old/new image IDs, OCI revision, config snapshots, and container inspection;
- Worker deployment/version JSON and R2 metadata before/after;
- Archigma API health/JWKS and authenticated registry results;
- canonical and legacy TLS checks;
- canary and public list/clone/verify/push/lock/audit results;
- timestamps, operator, decision, and any rollback reason.
