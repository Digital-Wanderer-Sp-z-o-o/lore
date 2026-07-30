// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { env } from "cloudflare:workers";
import { beforeEach, describe, expect, it } from "vitest";

import worker from "../src/index";

const encoder = new TextEncoder();

beforeEach(async () => {
  await env.DB.batch([
    env.DB.prepare("DELETE FROM lore_item_attributes"),
    env.DB.prepare("DELETE FROM lore_items"),
    env.DB.prepare("DELETE FROM lore_condition_guard"),
  ]);
});

describe("Lore D1 DynamoDB gateway", () => {
  it("reports D1 readiness", async () => {
    const response = await worker.fetch(new Request("https://lore-gateway.test/health"), env);
    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toMatchObject({ database: "ready" });
  });

  it("rejects unsigned requests", async () => {
    const response = await worker.fetch(
      new Request("https://lore-gateway.test/", { body: "{}", method: "POST" }),
      env,
    );
    expect(response.status).toBe(403);
    await expect(readJson(response)).resolves.toMatchObject({
      __type: expect.stringContaining("UnrecognizedClientException"),
    });
  });

  it("reports configured virtual tables as active", async () => {
    const response = await dynamo("DescribeTable", { TableName: env.METADATA_TABLE });
    expect(response.status).toBe(200);
    await expect(readJson(response)).resolves.toMatchObject({
      Table: { TableName: env.METADATA_TABLE, TableStatus: "ACTIVE" },
    });
  });

  it("stores, queries, batch-loads, and deletes fragment associations", async () => {
    const first = fragmentItem("AQID", "EBE=");
    const second = fragmentItem("AQID", "ECI=");
    expect((await dynamo("PutItem", { Item: first, TableName: env.FRAGMENTS_TABLE })).status).toBe(200);
    expect((await dynamo("PutItem", { Item: second, TableName: env.FRAGMENTS_TABLE })).status).toBe(200);

    const queryResponse = await dynamo("Query", {
      ExpressionAttributeNames: { "#pk": "hash", "#sk": "repository_context" },
      ExpressionAttributeValues: { ":hash": { B: "AQID" }, ":repository": { B: "EA==" } },
      KeyConditionExpression: "#pk = :hash and begins_with(#sk, :repository)",
      Limit: 1,
      TableName: env.FRAGMENTS_TABLE,
    });
    expect(queryResponse.status).toBe(200);
    await expect(readJson(queryResponse)).resolves.toMatchObject({ Count: 1, Items: [first] });

    const batchResponse = await dynamo("BatchGetItem", {
      RequestItems: {
        [env.FRAGMENTS_TABLE]: {
          Keys: [keyOf(first), keyOf(second)],
        },
      },
    });
    expect(batchResponse.status).toBe(200);
    const batchBody = (await readJson(batchResponse)) as {
      Responses: Record<string, unknown[]>;
    };
    expect(batchBody.Responses[env.FRAGMENTS_TABLE]).toHaveLength(2);

    expect(
      (await dynamo("DeleteItem", { Key: keyOf(first), TableName: env.FRAGMENTS_TABLE })).status,
    ).toBe(200);
    const getResponse = await dynamo("GetItem", {
      Key: keyOf(first),
      TableName: env.FRAGMENTS_TABLE,
    });
    await expect(readJson(getResponse)).resolves.toEqual({});
  });

  it("implements mutable compare-and-swap and returns the old item", async () => {
    const item = mutableItem("AQ==");
    const insert = await dynamo("PutItem", {
      ConditionExpression: "attribute_not_exists(repository_id) and attribute_not_exists(#k)",
      ExpressionAttributeNames: { "#k": "key" },
      Item: item,
      ReturnValuesOnConditionCheckFailure: "ALL_OLD",
      TableName: env.MUTABLE_TABLE,
    });
    expect(insert.status).toBe(200);

    const conflicting = await dynamo("PutItem", {
      ConditionExpression: "#v = :value",
      ExpressionAttributeNames: { "#v": "value" },
      ExpressionAttributeValues: { ":value": { B: "d3Jvbmc=" } },
      Item: { ...item, value: { B: "Ag==" } },
      ReturnValuesOnConditionCheckFailure: "ALL_OLD",
      TableName: env.MUTABLE_TABLE,
    });
    expect(conflicting.status).toBe(400);
    await expect(readJson(conflicting)).resolves.toMatchObject({
      Item: item,
      __type: expect.stringContaining("ConditionalCheckFailedException"),
    });
  });

  it("keeps multi-item lock writes atomic when a condition fails", async () => {
    const existing = lockItem("AQ==", "owner-a");
    expect(
      (
        await dynamo("PutItem", {
          Item: existing,
          TableName: env.LOCKS_TABLE,
        })
      ).status,
    ).toBe(200);

    const candidate = lockItem("Ag==", "owner-b");
    const response = await dynamo("TransactWriteItems", {
      TransactItems: [existing, candidate].map((Item) => ({
        Put: {
          ConditionExpression: "attribute_not_exists(#pk)",
          ExpressionAttributeNames: { "#pk": "hash" },
          Item,
          ReturnValuesOnConditionCheckFailure: "ALL_OLD",
          TableName: env.LOCKS_TABLE,
        },
      })),
    });
    expect(response.status).toBe(400);
    await expect(readJson(response)).resolves.toMatchObject({
      CancellationReasons: [
        { Code: "ConditionalCheckFailed", Item: existing },
        { Code: "None" },
      ],
      __type: expect.stringContaining("TransactionCanceledException"),
    });

    const candidateGet = await dynamo("GetItem", {
      Key: keyOfLock(candidate),
      TableName: env.LOCKS_TABLE,
    });
    await expect(readJson(candidateGet)).resolves.toEqual({});
  });

  it("queries mutable key ranges and lock secondary indexes", async () => {
    const mutableItems = [
      { ...mutableItem("AQ=="), key: { B: "AQ==" } },
      { ...mutableItem("Ag=="), key: { B: "Ag==" } },
    ];
    for (const Item of mutableItems) {
      expect((await dynamo("PutItem", { Item, TableName: env.MUTABLE_TABLE })).status).toBe(200);
    }

    const mutableQuery = await dynamo("Query", {
      ConsistentRead: true,
      ExpressionAttributeNames: { "#key": "key", "#repo": "repository_id" },
      ExpressionAttributeValues: {
        ":end": { B: "Af8=" },
        ":repo": { B: "EA==" },
        ":start": { B: "AQ==" },
      },
      KeyConditionExpression: "#repo = :repo AND #key BETWEEN :start AND :end",
      TableName: env.MUTABLE_TABLE,
    });
    await expect(readJson(mutableQuery)).resolves.toMatchObject({
      Count: 1,
      Items: [mutableItems[0]],
    });

    const lock = lockItem("Aw==", "owner-indexed");
    expect((await dynamo("PutItem", { Item: lock, TableName: env.LOCKS_TABLE })).status).toBe(200);
    const lockQuery = await dynamo("Query", {
      ExpressionAttributeNames: { "#pk": "ownerId" },
      ExpressionAttributeValues: { ":owner": { S: "owner-indexed" } },
      IndexName: "OwnerRepositoryBranchIndex",
      KeyConditionExpression: "#pk = :owner",
      TableName: env.LOCKS_TABLE,
    });
    await expect(readJson(lockQuery)).resolves.toMatchObject({ Count: 1, Items: [lock] });
  });
});

function fragmentItem(hash: string, repositoryContext: string) {
  return { hash: { B: hash }, repository_context: { B: repositoryContext } };
}

function mutableItem(value: string) {
  return {
    key: { B: "AQ==" },
    repository_id: { B: "EA==" },
    value: { B: value },
  };
}

function lockItem(hash: string, ownerId: string) {
  return {
    branch: { B: "IA==" },
    description: { S: `asset-${hash}` },
    hash: { B: hash },
    ownerId: { S: ownerId },
    repository: { B: "EA==" },
    repositoryBranch: { B: "ECA=" },
    timestamp: { S: "2026-07-30T12:00:00Z" },
  };
}

function keyOf(item: ReturnType<typeof fragmentItem>) {
  return { hash: item.hash, repository_context: item.repository_context };
}

function keyOfLock(item: ReturnType<typeof lockItem>) {
  return { hash: item.hash, repositoryBranch: item.repositoryBranch };
}

async function dynamo(operation: string, body: unknown): Promise<Response> {
  const bodyText = JSON.stringify(body);
  const request = await signedRequest(
    "https://lore-gateway.test/",
    `DynamoDB_20120810.${operation}`,
    bodyText,
  );
  return worker.fetch(request, env);
}

async function readJson(response: Response): Promise<unknown> {
  return JSON.parse(new TextDecoder().decode(await response.arrayBuffer()));
}

async function signedRequest(url: string, target: string, body: string): Promise<Request> {
  const amzDate = formatAmzDate(new Date());
  const date = amzDate.slice(0, 8);
  const region = "auto";
  const bodyHash = await sha256Hex(encoder.encode(body));
  const host = new URL(url).host;
  const canonicalHeaders =
    `content-type:application/x-amz-json-1.0\n` +
    `host:${host}\n` +
    `x-amz-content-sha256:${bodyHash}\n` +
    `x-amz-date:${amzDate}\n` +
    `x-amz-target:${target}\n`;
  const signedHeaders = "content-type;host;x-amz-content-sha256;x-amz-date;x-amz-target";
  const canonicalRequest = `POST\n/\n\n${canonicalHeaders}\n${signedHeaders}\n${bodyHash}`;
  const scope = `${date}/${region}/dynamodb/aws4_request`;
  const stringToSign = `AWS4-HMAC-SHA256\n${amzDate}\n${scope}\n${await sha256Hex(encoder.encode(canonicalRequest))}`;
  const signingKey = await deriveSigningKey(env.AUTH_SECRET_ACCESS_KEY, date, region);
  const signature = bytesToHex(
    new Uint8Array(await hmac(signingKey, encoder.encode(stringToSign))),
  );

  return new Request(url, {
    body,
    headers: {
      authorization:
        `AWS4-HMAC-SHA256 Credential=${env.AUTH_ACCESS_KEY_ID}/${scope}, ` +
        `SignedHeaders=${signedHeaders}, Signature=${signature}`,
      "content-type": "application/x-amz-json-1.0",
      "x-amz-content-sha256": bodyHash,
      "x-amz-date": amzDate,
      "x-amz-target": target,
    },
    method: "POST",
  });
}

function formatAmzDate(date: Date): string {
  return date.toISOString().replace(/[:-]|\.\d{3}/g, "");
}

async function deriveSigningKey(secret: string, date: string, region: string): Promise<ArrayBuffer> {
  const dateKey = await hmac(encoder.encode(`AWS4${secret}`), encoder.encode(date));
  const regionKey = await hmac(dateKey, encoder.encode(region));
  const serviceKey = await hmac(regionKey, encoder.encode("dynamodb"));
  return hmac(serviceKey, encoder.encode("aws4_request"));
}

async function hmac(key: BufferSource, data: BufferSource): Promise<ArrayBuffer> {
  const cryptoKey = await crypto.subtle.importKey(
    "raw",
    key,
    { hash: "SHA-256", name: "HMAC" },
    false,
    ["sign"],
  );
  return crypto.subtle.sign("HMAC", cryptoKey, data);
}

async function sha256Hex(value: BufferSource): Promise<string> {
  return bytesToHex(new Uint8Array(await crypto.subtle.digest("SHA-256", value)));
}

function bytesToHex(bytes: Uint8Array): string {
  return [...bytes].map((value) => value.toString(16).padStart(2, "0")).join("");
}
