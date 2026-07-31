<!-- SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o. -->
<!-- SPDX-License-Identifier: MIT -->

# Single-node Hetzner Lore staging

This directory runs the first scaling baseline from
[the reviewed architecture](../cloudflare/ARCHITECTURE.md). It is intentionally a staging setup:

- one Hetzner dedicated server terminates stock Lore QUIC/gRPC;
- a bounded local NVMe store caches immutable fragments and their successful association queries;
- R2 remains the durable payload store;
- SQLite Durable Objects own immutable metadata, mutable pointers, and lock coordination;
- there is no custom client data plane and no durable state depends on the Hetzner disk.

Do not ingest the production corpus until the native backend passes the complete correctness,
failure-injection, and distributed 80-client gates.

## Server shape

Start with one EX44-class dedicated server in Falkenstein or Helsinki: 64 GB RAM, two local NVMe
devices, and the standard dedicated 1 Gbit/s uplink. A simple RAID1 root filesystem leaves roughly
500 GB usable; this profile caps Lore's cache at 400 GiB. The cache is disposable, so a later host
may instead dedicate/stripe NVMe devices after a measured disk bottleneck.

Do not buy the 10 Gbit/s uplink before the 1 Gbit/s run shows sustained network saturation. One
server is a performance test target, not production high availability; add the second server before
team rollout.

An EX-series machine is a Robot/dedicated-server product. `hcloud` credentials manage Hetzner Cloud
VMs and cannot provision this server class. Provision it in Robot or with Robot API credentials,
record the exact hardware/price, and install current Debian or Ubuntu.

Hetzner `installimage` on a UEFI host requires an explicit EFI System Partition. For the two-disk
RAID1 layout used by this profile, use the equivalent of
`/boot/efi:esp:256M,/boot:ext4:1G,/:ext4:all`; omitting the ESP fails validation before
partitioning. After reboot, verify both arrays report `[UU]` in `/proc/mdstat` before accepting the
host.

## Private access and authentication

The checked-in profile requires repository-scoped JWTs issued from existing Rendermoon accounts.
Put client traffic on WireGuard and allow these ports only from the team VPN CIDR:

| Port | Protocol | Purpose |
| --- | --- | --- |
| 41337 | UDP | Lore QUIC |
| 41337 | TCP | Lore gRPC |
| 22 | TCP | administration, preferably only over WireGuard |

The HTTP health endpoint binds to loopback and is not public. Use `lore://` only inside this private
test network; that URL scheme intentionally skips certificate verification. JWT protects the Lore
application requests but does not encrypt this transport. Before non-staging access, configure a
trusted `lores://` certificate as well.

The checked-in staging profile binds QUIC and gRPC specifically to the server WireGuard address
`10.80.0.10`. Keep that address in sync with `wg0`; do not broaden the bind to `0.0.0.0`.

## Deploy

Install Docker Engine with the Compose plugin, clone this fork on the server, then from the repository
root run:

```bash
cd contrib/hetzner
cp .env.example .env
chmod 600 .env
```

Fill `LORE_CLOUDFLARE_SHARED_SECRET` in `.env` with the same value installed as the staging
Worker's `AUTH_SECRET_ACCESS_KEY`. The binding name is retained for bootstrap compatibility; it is
only the Worker HMAC secret and does not enable or call AWS. The server no longer receives R2 or
AWS-compatible credentials.

Authentication has a strict deployment order:

1. deploy the `rm-api` LORE auth adapter with its RSA signing-key secrets;
2. verify `https://rendermoon-staging.fly.dev/api/v1/lore/auth/jwks.json`;
3. register `urc-019fb946db5876239c2749aa8a51545d` and its initial owners through the Rendermoon
   admin API;
4. only then redeploy this Lore Server configuration.

Keep `[environment.endpoint].auth_url` on the `ucs-auth://` scheme. The client
uses that complete endpoint string as the token-store key, so advertising the
same host as `https://` after a `ucs-auth://` login makes a valid stored identity
look missing. The JWK endpoint remains a normal `https://` URL.

Deploying step 4 first makes Lore Server unable to fetch its configured JWKS. The Rendermoon
runbook is `docs/operations/lore-auth.md` in the Archigma repository.

Create the cache root and start Lore:

```bash
sudo install -d -m 0750 /var/lib/lore/cache
docker compose build
docker compose up -d
curl --fail http://127.0.0.1:41339/health_check
docker compose logs --tail 100 lore-server
```

After startup, log in with a Rendermoon API key (or omit the token options for the browser handoff):

```bash
lore auth login \
  --token-type api-key \
  --token "$RENDERMOON_LORE_API_KEY" \
  --auth-url ucs-auth://rendermoon-staging.fly.dev \
  --non-interactive
```

The Compose service uses host networking so the same port can serve QUIC on UDP and gRPC on TCP.
`config.toml` selects Lore's native composite store: `/data/cache` is the local tier and the native
Cloudflare plugin is the durable tier. The plugin calls a signed, versioned Worker API; the Worker
uses SQLite Durable Objects and a private R2 binding. Removing the container or cache directory must
not remove committed data.

## Scaling test

Use a representative Blender corpus and one fixed `.lore/view`. A full 500 GB checkout for 80 test
processes would require 40 TB on the load generators and is not the first test. Begin with a sparse
20-100 GB materialization, then run daily-delta and representative full-materialization tests.

Run the fan-out script from multiple load generators so one workstation's disk or NIC does not
become the result. For example, four machines can each supply 20 clients for the 80-client stage:

```powershell
.\contrib\hetzner\scale-clone.ps1 `
  -RemoteUrl lore://10.80.0.10:41337/blender-scale `
  -ViewFile D:\lore-tests\blender-scale.view `
  -Concurrency 1,10,20
```

Every client receives an isolated local store. The script records clone and verification duration,
exit codes, stdout, and stderr under a timestamped result directory. It stops the ladder on the first
failure or timeout. Start all load generators at the agreed wall-clock time for distributed stages.

Run two passes:

1. Cold server cache: set `LORE_CACHE_ROOT` in `.env` to a fresh empty directory, recreate the
   service, and run one concurrency stage. Repeat with a new directory for each cold stage.
2. Warm server cache: keep the cache root unchanged and rerun the identical corpus, view, and stages.

Do not delete old cache roots until their run is accepted; renaming or choosing a fresh directory
makes the reset recoverable. For every stage collect, at minimum:

```bash
docker stats archigma-lore-staging
sar -n DEV 1
iostat -xz 1
docker compose logs --since 10m lore-server
```

Record aggregate and per-client throughput, p50/p95/p99 completion time, process failures, Lore error
types, retry counts, server CPU/RAM/disk/network, cache size/hit behavior, DO/Worker errors, and R2
request/error metrics. Stop increasing concurrency on any false not-found, verification failure,
corruption, repeated timeout, or sustained resource saturation.

Use `lore-chaos-client parallel` separately for mixed-operation correctness. It has one writer and
multiple readers, but it is not a download-throughput benchmark:

```bash
lore-chaos-client parallel -r ./ChaosPlayground --runners 80 --time-limit-mins 60 --seed 1000
```

## Promotion gate

This profile can answer whether one Hetzner host, its NVMe cache, and a 1 Gbit/s uplink are adequate
for normal sparse/delta traffic. Production qualification still requires the conformance and
failure-injection gates in `contrib/cloudflare/ARCHITECTURE.md`, plus a second Lore server passing
host-loss and failover tests.
