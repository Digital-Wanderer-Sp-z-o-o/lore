// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import type {
  AddressDto,
  ApiErrorBody,
  LockRecoveryAuditCursorDto,
  LockQueryDto,
  LockResourceDto,
} from "./contracts";
import { ImmutableMetadataShard } from "./immutable";
import { LockCoordinator } from "./locks";
import { MutablePartitionStore } from "./mutable";
import {
  ValidationError,
  address,
  boundedArray,
  context,
  fragment,
  hash,
  keyType,
  record,
  storeMatch,
  stringField,
  uintField,
} from "./validation";

export { ImmutableMetadataShard, LockCoordinator, MutablePartitionStore };

const encoder = new TextEncoder();
const MAX_CLOCK_SKEW_SECONDS = 300;

export default {
  async fetch(request: Request, env: Cloudflare.Env): Promise<Response> {
    const started = Date.now();
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return Response.json({
        status: "ok",
        apiVersion: "v1",
        deployment: env.CF_VERSION_METADATA,
        durableObjects: "configured",
        capabilities: {
          lockRecoveryAudit: "v1",
          lockRecoveryOwnerCas: true,
        },
      });
    }

    const rawBody = await request.arrayBuffer();
    if (
      !(await authenticated(
        request,
        url.pathname,
        rawBody,
        env.AUTH_SECRET_ACCESS_KEY,
      ))
    ) {
      return errorResponse(401, "invalid_request", "invalid request signature");
    }

    try {
      if (url.pathname.startsWith("/v1/payload/")) {
        const response = await payloadRoute(
          request.method,
          url.pathname,
          rawBody,
          env,
        );
        console.log(
          JSON.stringify({
            event: "lore_payload_request",
            method: request.method,
            status: response.status,
            elapsedMs: Date.now() - started,
          }),
        );
        return response;
      }
      if (request.method !== "POST")
        return errorResponse(405, "invalid_request", "POST required");
      const input = record(JSON.parse(new TextDecoder().decode(rawBody)));
      const response = await route(url.pathname, input, env);
      console.log(
        JSON.stringify({
          event: "lore_do_request",
          path: url.pathname,
          status: response.status,
          elapsedMs: Date.now() - started,
        }),
      );
      return response;
    } catch (error) {
      const response = mapError(error);
      console.error(
        JSON.stringify({
          event: "lore_do_request_failed",
          path: url.pathname,
          status: response.status,
          elapsedMs: Date.now() - started,
          error: error instanceof Error ? error.message : String(error),
        }),
      );
      return response;
    }
  },
};

async function route(
  path: string,
  input: Record<string, unknown>,
  env: Cloudflare.Env,
): Promise<Response> {
  const maxBatch = Number.parseInt(env.MAX_BATCH_SIZE, 10);
  switch (path) {
    case "/v1/immutable/exist-batch": {
      const partition = context(input.partition, "partition");
      const requested = storeMatch(input.matchRequested);
      const addresses = boundedArray(
        input.addresses,
        maxBatch,
        "addresses",
      ).map(address);
      const matches = new Array<number>(addresses.length);
      const groups = groupAddresses(addresses);
      await Promise.all(
        [...groups.values()].map(async (entries) => {
          const stub = immutableStub(env, entries[0]?.address.hash ?? "");
          const shardMatches = await stub.existBatch(
            partition,
            entries.map((entry) => entry.address),
            requested,
          );
          entries.forEach((entry, index) => {
            matches[entry.index] = shardMatches[index] ?? 0;
          });
        }),
      );
      return Response.json({ matches });
    }
    case "/v1/immutable/query": {
      const partition = context(input.partition, "partition");
      const target = address(input.address);
      return Response.json(
        await immutableStub(env, target.hash).query(
          partition,
          target,
          storeMatch(input.matchRequested),
        ),
      );
    }
    case "/v1/immutable/put": {
      const partition = context(input.partition, "partition");
      const target = address(input.address);
      await immutableStub(env, target.hash).put(
        partition,
        target,
        fragment(input.fragment),
      );
      return Response.json({ ok: true });
    }
    case "/v1/immutable/associate": {
      const partition = context(input.partition, "partition");
      const target = address(input.address);
      await immutableStub(env, target.hash).associate(partition, target);
      return Response.json({ ok: true });
    }
    case "/v1/immutable/begin-obliteration": {
      const targetHash = hash(input.hash);
      return Response.json(
        await immutableStub(env, targetHash).beginObliteration(targetHash),
      );
    }
    case "/v1/immutable/remove-association": {
      const partition = context(input.partition, "partition");
      const target = address(input.address);
      return Response.json(
        await immutableStub(env, target.hash).removeAssociation(
          partition,
          target,
        ),
      );
    }
    case "/v1/immutable/cancel-obliteration": {
      const targetHash = hash(input.hash);
      await immutableStub(env, targetHash).cancelObliteration(
        targetHash,
        fragment(input.fragment),
      );
      return Response.json({ ok: true });
    }
    case "/v1/immutable/finish-obliteration": {
      const targetHash = hash(input.hash);
      await immutableStub(env, targetHash).finishObliteration(targetHash);
      return Response.json({ ok: true });
    }
    case "/v1/immutable/association-count": {
      const targetHash = hash(input.hash);
      return Response.json({
        count: await immutableStub(env, targetHash).associationCount(
          targetHash,
        ),
      });
    }
    case "/v1/mutable/load": {
      const partition = context(input.partition, "partition");
      return Response.json({
        value: await mutableStub(env, partition).load(
          hash(input.key, "key"),
          keyType(input.keyType),
        ),
      });
    }
    case "/v1/mutable/store": {
      const partition = context(input.partition, "partition");
      await mutableStub(env, partition).store(
        hash(input.key, "key"),
        hash(input.value, "value"),
        keyType(input.keyType),
      );
      return Response.json({ ok: true });
    }
    case "/v1/mutable/compare-and-swap": {
      const partition = context(input.partition, "partition");
      return Response.json(
        await mutableStub(env, partition).compareAndSwap(
          hash(input.key, "key"),
          hash(input.expected, "expected"),
          hash(input.value, "value"),
          keyType(input.keyType),
        ),
      );
    }
    case "/v1/mutable/list": {
      const partition = context(input.partition, "partition");
      return Response.json({
        entries: await mutableStub(env, partition).list(keyType(input.keyType)),
      });
    }
    case "/v1/locks/acquire": {
      const resources = lockResources(input.resources, maxBatch);
      const repository = context(input.repository, "repository");
      const result = await lockStub(env, repository).lockResources(
        stringField(input, "owner"),
        repository,
        resources,
        Date.now(),
        lockLeaseDurationMs(env),
      );
      return lockMutationResponse(result);
    }
    case "/v1/locks/release": {
      const resources = lockResources(input.resources, maxBatch);
      const repository = context(input.repository, "repository");
      const result = await lockStub(env, repository).unlockResources(
        stringField(input, "actor"),
        stringField(input, "expectedOwner"),
        repository,
        resources,
        Date.now(),
        lockLeaseDurationMs(env),
      );
      return lockMutationResponse(result);
    }
    case "/v1/locks/status": {
      const resources = lockResources(input.resources, maxBatch);
      const repository = context(input.repository, "repository");
      return Response.json({
        locks: await lockStub(env, repository).checkLocksStatus(
          repository,
          resources,
          Date.now(),
          lockLeaseDurationMs(env),
        ),
      });
    }
    case "/v1/locks/query": {
      const query = lockQuery(input.query);
      return Response.json({
        locks: await lockStub(env, repositoryForLockQuery(query)).queryLocks(
          query,
          Date.now(),
          lockLeaseDurationMs(env),
        ),
      });
    }
    case "/v1/locks/recovery-audit": {
      const repository = context(input.repository, "repository");
      return Response.json(
        await lockStub(env, repository).queryRecoveryAudit(
          repository,
          auditPageLimit(input),
          lockRecoveryAuditCursor(input.cursor),
        ),
      );
    }
    default:
      return errorResponse(404, "not_found", "unknown API route");
  }
}

async function payloadRoute(
  method: string,
  path: string,
  body: ArrayBuffer,
  env: Cloudflare.Env,
): Promise<Response> {
  const hashValue = hash(path.slice("/v1/payload/".length));
  const key = `payloads/v1/${hashValue}`;
  if (method === "PUT") {
    if (body.byteLength === 0 || body.byteLength > 256 * 1024) {
      throw new ValidationError(
        "payload must contain between 1 and 262144 bytes",
      );
    }
    const stored = await env.PAYLOADS.put(key, body, {
      onlyIf: { etagDoesNotMatch: "*" },
    });
    if (stored === null) {
      const existing = await env.PAYLOADS.get(key);
      if (existing === null || !(await sameBytes(existing.body, body))) {
        return errorResponse(
          409,
          "conflict",
          "immutable payload key already contains different bytes",
        );
      }
    }
    return Response.json({ ok: true });
  }
  if (method === "GET") {
    const object = await env.PAYLOADS.get(key);
    if (object === null)
      return errorResponse(404, "not_found", "payload does not exist");
    return new Response(object.body, {
      headers: {
        "content-length": object.size.toString(),
        "content-type": "application/octet-stream",
      },
    });
  }
  if (method === "DELETE") {
    await env.PAYLOADS.delete(key);
    return Response.json({ ok: true });
  }
  return errorResponse(
    405,
    "invalid_request",
    "payload route requires GET, PUT, or DELETE",
  );
}

async function sameBytes(
  stream: ReadableStream,
  expected: ArrayBuffer,
): Promise<boolean> {
  const actual = new Uint8Array(await new Response(stream).arrayBuffer());
  const wanted = new Uint8Array(expected);
  if (actual.length !== wanted.length) return false;
  let difference = 0;
  for (let index = 0; index < actual.length; index += 1) {
    difference |= (actual[index] ?? 0) ^ (wanted[index] ?? 0);
  }
  return difference === 0;
}

function immutableStub(env: Cloudflare.Env, hashValue: string) {
  const shard = hashValue.slice(-2);
  return env.IMMUTABLE_METADATA.get(
    env.IMMUTABLE_METADATA.idFromName(`v1:${shard}`),
  );
}

function mutableStub(env: Cloudflare.Env, partition: string) {
  return env.MUTABLE_PARTITIONS.get(
    env.MUTABLE_PARTITIONS.idFromName(`v1:${partition}`),
  );
}

function lockStub(env: Cloudflare.Env, repository: string) {
  return env.LOCK_COORDINATOR.getByName(`v2:repository:${repository}`);
}

function lockLeaseDurationMs(env: Cloudflare.Env): number {
  const seconds = Number.parseInt(env.LOCK_LEASE_SECONDS, 10);
  if (!Number.isSafeInteger(seconds) || seconds < 60 || seconds > 2_592_000) {
    throw new ValidationError(
      "LOCK_LEASE_SECONDS must be between 60 and 2592000",
    );
  }
  return seconds * 1_000;
}

function repositoryForLockQuery(query: LockQueryDto): string {
  switch (query.kind) {
    case "hashRepository":
    case "hashRepositoryBranch":
    case "ownerRepository":
    case "ownerRepositoryBranch":
    case "repository":
    case "repositoryBranch":
    case "repositoryBranchDescription":
      return query.repository;
    case "hash":
    case "owner":
      throw new ValidationError(
        "global lock queries are not supported by the sharded backend",
      );
  }
}

function groupAddresses(
  addresses: readonly AddressDto[],
): Map<string, { address: AddressDto; index: number }[]> {
  const groups = new Map<string, { address: AddressDto; index: number }[]>();
  addresses.forEach((value, index) => {
    const shard = value.hash.slice(-2);
    const entries = groups.get(shard) ?? [];
    entries.push({ address: value, index });
    groups.set(shard, entries);
  });
  return groups;
}

function lockResources(value: unknown, maxBatch: number): LockResourceDto[] {
  return boundedArray(value, maxBatch, "resources").map((candidate) => {
    const resource = record(candidate, "resource");
    const description = stringField(resource, "description");
    if (description.length > 4096)
      throw new ValidationError("lock description exceeds 4096 characters");
    return {
      branch: context(resource.branch, "branch"),
      hash: hash(resource.hash),
      description,
    };
  });
}

function lockQuery(value: unknown): LockQueryDto {
  const query = record(value, "query");
  const kind = stringField(query, "kind");
  switch (kind) {
    case "hash":
      return { kind, hash: hash(query.hash) };
    case "hashRepository":
      return {
        kind,
        hash: hash(query.hash),
        repository: context(query.repository, "repository"),
      };
    case "hashRepositoryBranch":
      return {
        kind,
        hash: hash(query.hash),
        repository: context(query.repository, "repository"),
        branch: context(query.branch, "branch"),
      };
    case "owner":
      return { kind, owner: stringField(query, "owner") };
    case "ownerRepository":
      return {
        kind,
        owner: stringField(query, "owner"),
        repository: context(query.repository, "repository"),
      };
    case "ownerRepositoryBranch":
      return {
        kind,
        owner: stringField(query, "owner"),
        repository: context(query.repository, "repository"),
        branch: context(query.branch, "branch"),
      };
    case "repository":
      return { kind, repository: context(query.repository, "repository") };
    case "repositoryBranch":
      return {
        kind,
        repository: context(query.repository, "repository"),
        branch: context(query.branch, "branch"),
      };
    case "repositoryBranchDescription":
      return {
        kind,
        repository: context(query.repository, "repository"),
        branch: context(query.branch, "branch"),
        description: stringField(query, "description"),
      };
    default:
      throw new ValidationError("unsupported lock query kind");
  }
}

function auditPageLimit(input: Record<string, unknown>): number {
  const limit = uintField(input, "limit", 100);
  if (limit === 0) throw new ValidationError("limit must be between 1 and 100");
  return limit;
}

function lockRecoveryAuditCursor(
  value: unknown,
): LockRecoveryAuditCursorDto | undefined {
  if (value === undefined || value === null) return undefined;
  const cursor = record(value, "cursor");
  const eventId = stringField(cursor, "eventId");
  if (
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[4-7][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
      eventId,
    )
  ) {
    throw new ValidationError("cursor eventId must be a UUID");
  }
  return {
    recordedAt: uintField(cursor, "recordedAt"),
    eventId,
  };
}

function lockMutationResponse(result: {
  status: string;
  locks?: readonly unknown[];
  resources?: readonly unknown[];
}): Response {
  if (result.status === "not_owned")
    return errorResponse(409, "conflict", "lock is owned by another user");
  if (result.status === "not_found")
    return errorResponse(404, "not_found", "lock does not exist");
  return Response.json(result);
}

async function authenticated(
  request: Request,
  path: string,
  body: ArrayBuffer,
  secret: string,
): Promise<boolean> {
  const timestamp = request.headers.get("x-lore-timestamp");
  const supplied = request.headers.get("x-lore-signature");
  if (
    timestamp === null ||
    supplied === null ||
    !/^\d+$/.test(timestamp) ||
    !/^[0-9a-f]{64}$/.test(supplied)
  )
    return false;
  if (
    Math.abs(Math.floor(Date.now() / 1000) - Number(timestamp)) >
    MAX_CLOCK_SKEW_SECONDS
  )
    return false;
  const bodyDigest = await crypto.subtle.digest("SHA-256", body);
  const message = `${timestamp}\n${request.method}\n${path}\n${hex(bodyDigest)}`;
  const key = await crypto.subtle.importKey(
    "raw",
    encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"],
  );
  return crypto.subtle.verify(
    "HMAC",
    key,
    fromHex(supplied),
    encoder.encode(message),
  );
}

function hex(value: ArrayBuffer): string {
  return [...new Uint8Array(value)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function fromHex(value: string): Uint8Array<ArrayBuffer> {
  const bytes = new Uint8Array(value.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

function mapError(error: unknown): Response {
  if (error instanceof ValidationError || error instanceof SyntaxError) {
    return errorResponse(400, "invalid_request", error.message);
  }
  const message = error instanceof Error ? error.message : String(error);
  if (message.startsWith("CONFLICT:"))
    return errorResponse(409, "conflict", message.slice(9).trim());
  if (message.startsWith("NOT_FOUND:"))
    return errorResponse(404, "not_found", message.slice(10).trim());
  // Unknown DO/network failures are retryable. They must never look like missing content.
  return errorResponse(503, "slow_down", "temporary backend failure");
}

function errorResponse(
  status: number,
  error: ApiErrorBody["error"],
  message: string,
): Response {
  return Response.json({ error, message } satisfies ApiErrorBody, { status });
}
