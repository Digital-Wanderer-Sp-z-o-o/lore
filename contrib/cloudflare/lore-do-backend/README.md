<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Lore Cloudflare Durable Objects backend

This Worker is the native Cloudflare service boundary for Lore Server. It does not emulate
DynamoDB and has no D1 binding.

- `ImmutableMetadataShard`: 256 logical SQLite Durable Objects selected by the final hash byte.
- `MutablePartitionStore`: one SQLite Durable Object per Lore partition, including the null catalog partition.
- `LockCoordinator`: one SQLite Durable Object per repository, so multi-resource acquire/release
  and administrative-recovery audit writes stay atomic without serializing unrelated repositories.
- `PAYLOADS`: private R2 binding using keys `payloads/v1/<blake3-hash>`.

All non-health requests require an HMAC-SHA256 signature over timestamp, HTTP method, path, and
SHA-256 body digest. Unknown Worker/DO/R2 failures return retryable `slow_down`; they are never
translated into missing content.

Successful foreign-owner releases append an immutable recovery event in the same SQLite
transaction that removes the locks. Failed owner comparisons and normal self-unlock calls do not
create recovery events. The signed `/v1/locks/recovery-audit` route exposes the repository-scoped
history in stable newest-first cursor pages; it is a backend surface and does not authorize end
users by itself.

Audited obliteration uses an additive immutable-shard schema migration. Starting an operation
stores its actor, correlation ID, repository, address, original fragment metadata, and recovery
stage in the same SQLite transaction that marks the fragment as obliterating. Association removal
and final metadata changes update that durable lifecycle record atomically. If Lore Server or R2
communication stops between phases, the next request resumes from the recorded stage; shared R2
payloads are retained while any association remains. The original obliteration routes remain
available only for old-server compatibility during the ordered Worker-before-server rollout. New
servers use the explicitly audited routes, and legacy route traffic must remain zero before the
feature is enabled for users.

## Local validation

The development toolchain overrides `undici` to `7.29.0` because the pinned
Wrangler/Miniflare release otherwise resolves the vulnerable `7.28.0`. Keep
the override until the upstream dependency resolves at least `7.29.0`, and do
not change it without a clean `npm audit` plus the Worker runtime tests. The
staging preflight enforces the high-severity audit gate.

```bash
npm install
npm run check
npm test
npm run preflight:staging
```

## Staging deployment

Create a random secret of at least 32 characters and install it without committing it:

```bash
npx wrangler secret put AUTH_SECRET_ACCESS_KEY --env staging
```

Do not use `wrangler deploy` for this Worker. It couples upload and immediate
100% activation. Follow [the staging rollout runbook](../STAGING_ROLLOUT.md):
upload an immutable version, add it to a 0% deployment, prove its exact version
and old-RPC compatibility through a signed version-override smoke, then promote
it and prove the new audit RPCs without an override. A version override does
not pin Durable Object RPC calls to the same code version, so existing method
names, signatures, and legacy HTTP shapes remain compatible throughout the
rolling deployment. The runbook records the previous version and separates
Worker, server, and client rollback decisions.

Install the same value as `LORE_CLOUDFLARE_SHARED_SECRET` in the Hetzner Compose `.env`, build the
exact reviewed Lore commit, and recreate the service. The old `lore-d1-gateway` is a retained POC;
it is not referenced by the target Hetzner configuration.
