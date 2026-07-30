<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Lore D1 gateway

This Worker exposes the small DynamoDB JSON API subset used by Lore and stores
the data in Cloudflare D1. Requests must be AWS SigV4-signed for the `dynamodb`
service; unsigned requests and unsupported operations are rejected.

The gateway supports Lore's fragment indexes, metadata, mutable compare-and-swap
operations, and atomic lock transactions. R2 payload traffic does not pass
through this Worker.

## Local checks

```powershell
npm ci
npm run check
npm test
```

`.dev.vars.example` documents the local signing secret. Copy it to `.dev.vars`
and never commit the resulting file.

## Staging deployment

Create the D1 database once, put its ID in `wrangler.jsonc`, then run:

```powershell
npx wrangler d1 migrations apply DB --env staging --remote
npx wrangler secret put AUTH_SECRET_ACCESS_KEY --env staging
npm run deploy:staging
```

`AUTH_SECRET_ACCESS_KEY` must equal `AWS_SECRET_ACCESS_KEY` on the Fly app.
`AUTH_ACCESS_KEY_ID` is non-secret and must equal Fly's `AWS_ACCESS_KEY_ID`.
