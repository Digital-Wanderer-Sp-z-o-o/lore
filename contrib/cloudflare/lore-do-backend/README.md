<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Lore Cloudflare Durable Objects backend

This Worker is the native Cloudflare service boundary for Lore Server. It does not emulate
DynamoDB and has no D1 binding.

- `ImmutableMetadataShard`: 256 logical SQLite Durable Objects selected by the final hash byte.
- `MutablePartitionStore`: one SQLite Durable Object per Lore partition, including the null catalog partition.
- `LockCoordinator`: one SQLite Durable Object so multi-resource acquire/release stays atomic.
- `PAYLOADS`: private R2 binding using keys `payloads/v1/<blake3-hash>`.

All non-health requests require an HMAC-SHA256 signature over timestamp, HTTP method, path, and
SHA-256 body digest. Unknown Worker/DO/R2 failures return retryable `slow_down`; they are never
translated into missing content.

## Local validation

```bash
npm install
npm run check
npm test
npx wrangler deploy --dry-run --env staging
```

## Staging deployment

Create a random secret of at least 32 characters and install it without committing it:

```bash
npx wrangler secret put AUTH_SECRET_ACCESS_KEY --env staging
npm run deploy:staging
```

Install the same value as `LORE_CLOUDFLARE_SHARED_SECRET` in the Hetzner Compose `.env`, build the
exact reviewed Lore commit, and recreate the service. The old `lore-d1-gateway` is a retained POC;
it is not referenced by the target Hetzner configuration.
