<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Private, stateless Lore staging on Fly.io

> [!WARNING]
> This is a staging proof of concept, not the proposed 10-50 TB production architecture. The D1
> DynamoDB compatibility gateway has already shown hot-path latency and incorrect transient
> not-found behavior under concurrent clone load. See the
> [reviewed Cloudflare/Hetzner architecture](../cloudflare/ARCHITECTURE.md) before extending it.

Lore runs as a stateless Fly Machine in Frankfurt and has no Fly volume. Cloudflare owns all
durable state:

- R2 stores immutable fragment payloads through its S3-compatible API.
- The `archigma-lore-d1-staging` D1 database stores fragment indexes, metadata, mutable refs, and
  distributed locks.
- The `archigma-lore-d1-staging` Worker verifies AWS SigV4 requests from Lore and translates the
  DynamoDB operations Lore uses into D1 transactions.

R2 payloads travel directly between Lore and R2; the Worker is only the metadata gateway. No AWS
service is required. Lore still uses its existing AWS SDK internally for the S3 and DynamoDB wire
protocols.

The app intentionally has no public Fly service. Lore starts without JWT verification unless it is
connected to an external authentication service, so team members reach it through the Fly private
network:

```text
lore://archigma-lore-staging.internal:41337/<repository-name>
```

The `lore://` scheme skips server certificate verification and is suitable only on this private
staging network. Before public exposure, configure a durable certificate and Lore's UCS/JWT
authentication integration.

## Cloudflare gateway

The Worker source, D1 migration, tests, and deployment commands live in
`contrib/cloudflare/lore-d1-gateway`. Its `AUTH_SECRET_ACCESS_KEY` secret must equal the Fly app's
`AWS_SECRET_ACCESS_KEY`; `AUTH_ACCESS_KEY_ID` and `AWS_ACCESS_KEY_ID` must both be
`archigma-lore-staging`.

The R2 credentials are separate Fly secrets so they can be restricted to the dedicated
`archigma-lore-staging` bucket:

```powershell
flyctl secrets set --app archigma-lore-staging `
  R2_ACCESS_KEY_ID="..." `
  R2_SECRET_ACCESS_KEY="..." `
  AWS_ACCESS_KEY_ID="archigma-lore-staging" `
  AWS_SECRET_ACCESS_KEY="<same value as the Worker secret>"
```

Do not reuse account-wide R2 credentials.

## Deploy Lore

From the repository root:

```powershell
flyctl deploy . --config contrib/fly/fly.toml
```

`contrib/fly/config.toml` is baked into the image as the `fly` environment profile. It contains
only resource names and endpoints; credentials remain Fly and Worker secrets. The Machine root
filesystem is disposable, so scaling or replacement does not require attaching storage.
