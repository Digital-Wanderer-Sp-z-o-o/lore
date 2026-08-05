// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { SELF, env } from "cloudflare:test";
import { describe, expect, it } from "vitest";

const SECRET = "test-secret";
const PARTITION = "11".repeat(16);
const OTHER_PARTITION = "22".repeat(16);
const CONTEXT = "33".repeat(16);
const OTHER_CONTEXT = "44".repeat(16);
const ZERO_HASH = "00".repeat(32);

describe("Lore Durable Objects backend", () => {
  it("requires a fresh valid HMAC signature", async () => {
    const unsigned = await SELF.fetch("https://lore.test/v1/mutable/load", {
      method: "POST",
      body: JSON.stringify({ partition: PARTITION, key: hash(1), keyType: 0 }),
    });
    expect(unsigned.status).toBe(401);

    const stale = await api(
      "/v1/mutable/load",
      {
        partition: PARTITION,
        key: hash(1),
        keyType: 0,
      },
      Math.floor(Date.now() / 1000) - 600,
    );
    expect(stale.status).toBe(401);
  });

  it("preserves ordered match specificity across immutable shards", async () => {
    const first = { hash: hash(1), context: CONTEXT };
    const second = { hash: hash(2), context: CONTEXT };
    const fragment = { flags: 0, sizePayload: 16, sizeContent: 16 };
    expect(
      (
        await api("/v1/immutable/put", {
          partition: PARTITION,
          address: first,
          fragment,
        })
      ).status,
    ).toBe(200);
    expect(
      (
        await api("/v1/immutable/put", {
          partition: PARTITION,
          address: second,
          fragment,
        })
      ).status,
    ).toBe(200);

    const response = await api("/v1/immutable/exist-batch", {
      partition: PARTITION,
      addresses: [
        second,
        { hash: first.hash, context: OTHER_CONTEXT },
        { hash: hash(3), context: CONTEXT },
      ],
      matchRequested: 3,
    });
    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toEqual({ matches: [3, 2, 0] });

    const crossPartition = await api("/v1/immutable/query", {
      partition: OTHER_PARTITION,
      address: first,
      matchRequested: 3,
    });
    await expect(crossPartition.json()).resolves.toMatchObject({
      matchMade: 1,
    });
  });

  it("allows one mutable CAS winner and deletes on the null hash", async () => {
    const key = hash(20);
    const requests = [hash(21), hash(22)].map((value) =>
      api("/v1/mutable/compare-and-swap", {
        partition: PARTITION,
        key,
        expected: ZERO_HASH,
        value,
        keyType: 3,
      }),
    );
    const responses = await Promise.all(requests);
    const bodies = await Promise.all(
      responses.map(
        async (response) => response.json() as Promise<{ swapped: boolean }>,
      ),
    );
    expect(bodies.filter((body) => body.swapped)).toHaveLength(1);

    const loaded = await api("/v1/mutable/load", {
      partition: PARTITION,
      key,
      keyType: 3,
    });
    const loadedBody = (await loaded.json()) as { value: string };
    expect([hash(21), hash(22)]).toContain(loadedBody.value);

    await api("/v1/mutable/store", {
      partition: PARTITION,
      key,
      value: ZERO_HASH,
      keyType: 3,
    });
    await expect(
      (
        await api("/v1/mutable/load", { partition: PARTITION, key, keyType: 3 })
      ).json(),
    ).resolves.toEqual({ value: null });
  });

  it("stores payloads privately under the signed service API", async () => {
    const payloadHash = hash(30);
    const payload = new Uint8Array([1, 2, 3, 4]);
    expect(
      (await rawApi("PUT", `/v1/payload/${payloadHash}`, payload)).status,
    ).toBe(200);
    expect(
      (await rawApi("PUT", `/v1/payload/${payloadHash}`, payload)).status,
    ).toBe(200);
    expect(
      (
        await rawApi(
          "PUT",
          `/v1/payload/${payloadHash}`,
          new Uint8Array([9, 9]),
        )
      ).status,
    ).toBe(409);

    const loaded = await rawApi(
      "GET",
      `/v1/payload/${payloadHash}`,
      new Uint8Array(),
    );
    expect(loaded.status).toBe(200);
    expect(new Uint8Array(await loaded.arrayBuffer())).toEqual(payload);

    expect(
      (await rawApi("DELETE", `/v1/payload/${payloadHash}`, new Uint8Array()))
        .status,
    ).toBe(200);
    expect(
      (await rawApi("GET", `/v1/payload/${payloadHash}`, new Uint8Array()))
        .status,
    ).toBe(404);
  });

  it("keeps conflicting lock batches all-or-nothing", async () => {
    const held = resource(40, "held.blend");
    const candidate = resource(41, "candidate.blend");
    expect(
      (
        await api("/v1/locks/acquire", {
          owner: "alice",
          repository: PARTITION,
          resources: [held],
        })
      ).status,
    ).toBe(200);

    const conflict = await api("/v1/locks/acquire", {
      owner: "bob",
      repository: PARTITION,
      resources: [candidate, held],
    });
    expect(conflict.status).toBe(409);

    const status = await api("/v1/locks/status", {
      repository: PARTITION,
      resources: [candidate, held],
    });
    const body = (await status.json()) as {
      locks: { owner: string; resource: { hash: string } }[];
    };
    expect(body.locks).toHaveLength(1);
    expect(body.locks[0]).toMatchObject({
      owner: "alice",
      resource: { hash: held.hash },
    });
  });

  it("shards lock coordination by repository", async () => {
    const shared = resource(42, "shared.blend");
    expect(
      (
        await api("/v1/locks/acquire", {
          owner: "alice",
          repository: PARTITION,
          resources: [shared],
        })
      ).status,
    ).toBe(200);
    expect(
      (
        await api("/v1/locks/acquire", {
          owner: "bob",
          repository: OTHER_PARTITION,
          resources: [shared],
        })
      ).status,
    ).toBe(200);

    const first = await api("/v1/locks/query", {
      query: { kind: "repository", repository: PARTITION },
    });
    const second = await api("/v1/locks/query", {
      query: { kind: "repository", repository: OTHER_PARTITION },
    });
    const firstBody = (await first.json()) as {
      locks: { owner: string; resource: { hash: string } }[];
    };
    const secondBody = (await second.json()) as {
      locks: { owner: string; resource: { hash: string } }[];
    };
    expect(firstBody.locks).toContainEqual(
      expect.objectContaining({
        owner: "alice",
        resource: expect.objectContaining({ hash: shared.hash }),
      }),
    );
    expect(secondBody.locks).toContainEqual(
      expect.objectContaining({
        owner: "bob",
        resource: expect.objectContaining({ hash: shared.hash }),
      }),
    );
  });

  it("exposes durable recovery audit through the signed repository route", async () => {
    const recovered = resource(51, "recovered.blend");
    expect(
      (
        await api("/v1/locks/acquire", {
          owner: "alice",
          repository: OTHER_PARTITION,
          resources: [recovered],
        })
      ).status,
    ).toBe(200);
    expect(
      (
        await api("/v1/locks/release", {
          actor: "admin",
          expectedOwner: "alice",
          repository: OTHER_PARTITION,
          resources: [recovered],
        })
      ).status,
    ).toBe(200);

    const response = await api("/v1/locks/recovery-audit", {
      repository: OTHER_PARTITION,
      limit: 10,
    });
    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toMatchObject({
      events: [
        {
          actor: "admin",
          expectedOwner: "alice",
          repository: OTHER_PARTITION,
          resources: [recovered],
        },
      ],
    });
  });

  it("renews owned lock leases and expires inactive locks atomically", async () => {
    const repository = PARTITION;
    const locked = resource(43, "lease.blend");
    const stub = env.LOCK_COORDINATOR.getByName("lease-contract");

    await expect(
      stub.lockResources("alice", repository, [locked], 1_000, 1_000),
    ).resolves.toMatchObject({
      status: "ok",
      locks: [{ owner: "alice", lockedAt: 1_000 }],
    });
    await expect(
      stub.lockResources("alice", repository, [locked], 1_500, 1_000),
    ).resolves.toEqual({
      status: "ok",
      locks: [],
    });
    await expect(
      stub.checkLocksStatus(repository, [locked], 2_499, 1_000),
    ).resolves.toHaveLength(1);
    await expect(
      stub.checkLocksStatus(repository, [locked], 2_500, 1_000),
    ).resolves.toEqual([]);
    await expect(
      stub.lockResources("bob", repository, [locked], 2_500, 1_000),
    ).resolves.toMatchObject({
      status: "ok",
      locks: [{ owner: "bob", lockedAt: 2_500 }],
    });
  });

  it("does not partially renew a conflicting lock batch", async () => {
    const repository = PARTITION;
    const owned = resource(44, "owned.blend");
    const foreign = resource(45, "foreign.blend");
    const stub = env.LOCK_COORDINATOR.getByName("lease-conflict-contract");

    await stub.lockResources("alice", repository, [owned], 1_000, 1_000);
    await stub.lockResources("bob", repository, [foreign], 1_000, 1_000);
    await expect(
      stub.lockResources("alice", repository, [owned, foreign], 1_500, 1_000),
    ).resolves.toEqual({ status: "not_owned" });

    const locks = await stub.checkLocksStatus(
      repository,
      [owned, foreign],
      1_999,
      1_000,
    );
    expect(locks).toEqual([
      expect.objectContaining({ owner: "alice", lockedAt: 1_000 }),
      expect.objectContaining({ owner: "bob", lockedAt: 1_000 }),
    ]);
  });

  it("releases an administrative batch only for the expected owner", async () => {
    const repository = PARTITION;
    const first = resource(46, "first.blend");
    const second = resource(47, "second.blend");
    const stub = env.LOCK_COORDINATOR.getByName("owner-cas-contract");

    await stub.lockResources(
      "alice",
      repository,
      [first, second],
      1_000,
      10_000,
    );
    await expect(
      stub.unlockResources(
        "admin",
        "bob",
        repository,
        [first, second],
        1_500,
        10_000,
      ),
    ).resolves.toEqual({ status: "not_owned" });
    await expect(
      stub.queryRecoveryAudit(repository, 10),
    ).resolves.toEqual({ events: [] });
    await expect(
      stub.checkLocksStatus(repository, [first, second], 1_500, 10_000),
    ).resolves.toHaveLength(2);

    await expect(
      stub.unlockResources(
        "admin",
        "alice",
        repository,
        [first, second],
        1_500,
        10_000,
      ),
    ).resolves.toMatchObject({ status: "ok", resources: [first, second] });
    await expect(
      stub.checkLocksStatus(repository, [first, second], 1_500, 10_000),
    ).resolves.toEqual([]);

    const firstPage = await stub.queryRecoveryAudit(repository, 1);
    expect(firstPage.events).toEqual([
      expect.objectContaining({
        actor: "admin",
        expectedOwner: "alice",
        repository,
        recordedAt: 1_500,
        resources: [first, second],
      }),
    ]);
    expect(firstPage.nextCursor).toBeUndefined();

    const ownLock = resource(48, "admin-owned.blend");
    await stub.lockResources("admin", repository, [ownLock], 1_600, 10_000);
    await stub.unlockResources(
      "admin",
      "admin",
      repository,
      [ownLock],
      1_700,
      10_000,
    );
    await expect(stub.queryRecoveryAudit(repository, 10)).resolves.toEqual(
      firstPage,
    );
  });

  it("paginates durable administrative recovery events newest first", async () => {
    const repository = PARTITION;
    const first = resource(49, "first-recovery.blend");
    const second = resource(50, "second-recovery.blend");
    const stub = env.LOCK_COORDINATOR.getByName("recovery-audit-pagination");

    await stub.lockResources("alice", repository, [first], 1_000, 10_000);
    await stub.unlockResources(
      "admin",
      "alice",
      repository,
      [first],
      1_500,
      10_000,
    );
    await stub.lockResources("bob", repository, [second], 1_600, 10_000);
    await stub.unlockResources(
      "admin",
      "bob",
      repository,
      [second],
      1_700,
      10_000,
    );

    const firstPage = await stub.queryRecoveryAudit(repository, 1);
    expect(firstPage.events).toEqual([
      expect.objectContaining({ expectedOwner: "bob", resources: [second] }),
    ]);
    expect(firstPage.nextCursor).toBeDefined();
    const secondPage = await stub.queryRecoveryAudit(
      repository,
      1,
      firstPage.nextCursor,
    );
    expect(secondPage.events).toEqual([
      expect.objectContaining({ expectedOwner: "alice", resources: [first] }),
    ]);
    expect(secondPage.nextCursor).toBeUndefined();
  });
});

function hash(byte: number): string {
  return byte.toString(16).padStart(2, "0").repeat(32);
}

function resource(byte: number, description: string) {
  return { branch: CONTEXT, hash: hash(byte), description };
}

async function api(
  path: string,
  body: unknown,
  timestamp = Math.floor(Date.now() / 1000),
): Promise<Response> {
  const encoded = JSON.stringify(body);
  return rawApi(
    "POST",
    path,
    new TextEncoder().encode(encoded),
    timestamp,
    "application/json",
  );
}

async function rawApi(
  method: string,
  path: string,
  body: Uint8Array<ArrayBufferLike>,
  timestamp = Math.floor(Date.now() / 1000),
  contentType = "application/octet-stream",
): Promise<Response> {
  const bodyBytes = Uint8Array.from(body);
  const bodyDigest = await crypto.subtle.digest("SHA-256", bodyBytes);
  const message = `${timestamp}\n${method}\n${path}\n${hex(bodyDigest)}`;
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(SECRET),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const signature = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(message),
  );
  const init: RequestInit = {
    method,
    headers: {
      "content-type": contentType,
      "x-lore-signature": hex(signature),
      "x-lore-timestamp": timestamp.toString(),
    },
    ...(bodyBytes.length === 0 ? {} : { body: bodyBytes }),
  };
  return SELF.fetch(`https://lore.test${path}`, init);
}

function hex(value: ArrayBuffer): string {
  return [...new Uint8Array(value)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}
