<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Private Lore staging on Fly.io

This deployment runs Lore on a Fly Machine in Frankfurt (Fly's nearest currently available region
to Poland) and stores its initial pilot data on a
persistent Fly volume. It intentionally defines no public Fly service: upstream Lore starts
without JWT verification unless it is connected to an external authentication service. Team
members reach the server over Fly's private WireGuard network.

The target production storage layout is Cloudflare R2 for immutable payloads plus DynamoDB for
fragment indexes, mutable state, and locks. Lore cannot run entirely on Workers because its client
protocol requires inbound QUIC/UDP and gRPC/TCP. R2 also cannot replace the DynamoDB data model.

## Deploy the private pilot

From the repository root:

```powershell
flyctl apps create archigma-lore-staging --org personal
flyctl volumes create lore_data --app archigma-lore-staging --region fra --size 20
flyctl deploy . --config contrib/fly/fly.toml
```

Connect a workstation to the Fly organization with WireGuard, then use the app's private hostname:

```text
lore://archigma-lore-staging.internal:41337/<repository-name>
```

The `lore://` scheme is suitable only inside this private pilot network because it skips server
certificate verification. Before exposing Lore publicly, configure a durable certificate and the
server's UCS/JWT authentication integration.

## Switch immutable payloads to R2

`r2-dynamodb.toml.example` shows the complete store configuration. The fork adds two R2-specific
capabilities:

- S3 credentials can come from R2-specific environment variables while DynamoDB keeps using the
  normal AWS credential chain.
- `s3_object_versioning = "disabled"` deletes the current object directly instead of calling
  `ListObjectVersions`, which R2 does not implement.

Provision the R2 bucket and four DynamoDB tables first. Then make the config available in the image
or through Lore's `LORE__...` environment overrides, and set secrets on the Fly app:

```powershell
flyctl secrets set --app archigma-lore-staging `
  R2_ACCESS_KEY_ID="..." `
  R2_SECRET_ACCESS_KEY="..." `
  AWS_ACCESS_KEY_ID="..." `
  AWS_SECRET_ACCESS_KEY="..."
```

Do not reuse a general application bucket or account-wide credentials for the Lore staging app.
Use a dedicated R2 bucket and least-privilege keys scoped to that bucket.
