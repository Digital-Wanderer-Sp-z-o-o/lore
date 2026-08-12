// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import type {
  AttributeValue,
  BatchGetItemRequest,
  ConditionSpec,
  DynamoItem,
  QueryRequest,
  WriteSpec,
} from "./dynamo-types";
import { RequestError, validationError } from "./errors";
import type { LoreD1Repository } from "./repository";

const TARGET_PREFIX = "DynamoDB_20120810.";
const MAX_REQUEST_BODY_BYTES = 2 * 1024 * 1024;

export async function handleDynamoRequest(
  target: string,
  body: ArrayBuffer,
  repository: LoreD1Repository,
): Promise<unknown> {
  if (body.byteLength > MAX_REQUEST_BODY_BYTES) {
    throw validationError(`Request body exceeds ${MAX_REQUEST_BODY_BYTES} bytes`);
  }
  if (!target.startsWith(TARGET_PREFIX)) {
    throw new RequestError(400, "UnknownOperationException", `Unknown target: ${target}`);
  }
  const operation = target.slice(TARGET_PREFIX.length);
  const request = parseRequestBody(body);

  switch (operation) {
    case "BatchGetItem":
      return batchGetItem(parseBatchGetItem(request), repository);
    case "DeleteItem":
      return deleteItem(request, repository);
    case "DescribeTable":
      return describeTable(request, repository);
    case "GetItem":
      return getItem(request, repository);
    case "PutItem":
      return putItem(request, repository);
    case "Query":
      return query(parseQuery(request), repository);
    case "TransactWriteItems":
      return transactWriteItems(request, repository);
    default:
      throw new RequestError(
        400,
        "UnknownOperationException",
        `Operation is not implemented by the Lore D1 gateway: ${operation}`,
      );
  }
}

async function describeTable(
  request: Readonly<Record<string, unknown>>,
  repository: LoreD1Repository,
): Promise<unknown> {
  const tableName = requiredString(request, "TableName");
  if (!repository.hasTable(tableName)) {
    throw new RequestError(
      400,
      "ResourceNotFoundException",
      `Requested resource not found: Table: ${tableName} not found`,
    );
  }
  return {
    Table: {
      ItemCount: 0,
      TableName: tableName,
      TableSizeBytes: 0,
      TableStatus: "ACTIVE",
    },
  };
}

async function getItem(
  request: Readonly<Record<string, unknown>>,
  repository: LoreD1Repository,
): Promise<unknown> {
  const tableName = requiredString(request, "TableName");
  const item = await repository.get(tableName, requiredItem(request, "Key"));
  return item === undefined ? {} : { Item: item };
}

async function batchGetItem(
  request: BatchGetItemRequest,
  repository: LoreD1Repository,
): Promise<unknown> {
  const responses: Record<string, readonly DynamoItem[]> = {};
  for (const [tableName, keysAndAttributes] of Object.entries(request.RequestItems)) {
    responses[tableName] = await repository.batchGet(tableName, keysAndAttributes.Keys);
  }
  return { Responses: responses, UnprocessedKeys: {} };
}

async function putItem(
  request: Readonly<Record<string, unknown>>,
  repository: LoreD1Repository,
): Promise<unknown> {
  await repository.put(
    requiredString(request, "TableName"),
    requiredItem(request, "Item"),
    parseCondition(request),
  );
  return {};
}

async function deleteItem(
  request: Readonly<Record<string, unknown>>,
  repository: LoreD1Repository,
): Promise<unknown> {
  await repository.delete(
    requiredString(request, "TableName"),
    requiredItem(request, "Key"),
  );
  return {};
}

async function query(request: QueryRequest, repository: LoreD1Repository): Promise<unknown> {
  const result = await repository.query(request);
  return {
    Count: result.count,
    ScannedCount: result.count,
    ...(result.items === undefined ? {} : { Items: result.items }),
    ...(result.lastEvaluatedKey === undefined
      ? {}
      : { LastEvaluatedKey: result.lastEvaluatedKey }),
  };
}

async function transactWriteItems(
  request: Readonly<Record<string, unknown>>,
  repository: LoreD1Repository,
): Promise<unknown> {
  const rawItems = requiredArray(request, "TransactItems");
  const writes = rawItems.map((item, index) => parseWriteSpec(item, index));
  await repository.transactWrite(writes);
  return {};
}

function parseBatchGetItem(
  request: Readonly<Record<string, unknown>>,
): BatchGetItemRequest {
  const rawRequestItems = requiredRecord(request, "RequestItems");
  const requestItems: Record<string, { ConsistentRead?: boolean; Keys: readonly DynamoItem[] }> = {};
  for (const [tableName, rawKeysAndAttributes] of Object.entries(rawRequestItems)) {
    const keysAndAttributes = asRecord(rawKeysAndAttributes, `RequestItems.${tableName}`);
    const keys = requiredArray(keysAndAttributes, "Keys").map((key, index) =>
      asItem(key, `RequestItems.${tableName}.Keys[${index}]`),
    );
    const consistentRead = optionalBoolean(keysAndAttributes, "ConsistentRead");
    requestItems[tableName] = {
      Keys: keys,
      ...(consistentRead === undefined ? {} : { ConsistentRead: consistentRead }),
    };
  }
  return { RequestItems: requestItems };
}

function parseQuery(request: Readonly<Record<string, unknown>>): QueryRequest {
  const consistentRead = optionalBoolean(request, "ConsistentRead");
  const exclusiveStartKey = optionalItem(request, "ExclusiveStartKey");
  const expressionAttributeNames = optionalStringMap(request, "ExpressionAttributeNames");
  const filterExpression = optionalString(request, "FilterExpression");
  const indexName = optionalString(request, "IndexName");
  const limit = optionalNumber(request, "Limit");
  const select = optionalString(request, "Select");
  if (select !== undefined && select !== "ALL_ATTRIBUTES" && select !== "COUNT") {
    throw validationError(`Unsupported Select value: ${select}`);
  }
  return {
    ExpressionAttributeValues: requiredItem(request, "ExpressionAttributeValues"),
    KeyConditionExpression: requiredString(request, "KeyConditionExpression"),
    TableName: requiredString(request, "TableName"),
    ...(consistentRead === undefined ? {} : { ConsistentRead: consistentRead }),
    ...(exclusiveStartKey === undefined ? {} : { ExclusiveStartKey: exclusiveStartKey }),
    ...(expressionAttributeNames === undefined
      ? {}
      : { ExpressionAttributeNames: expressionAttributeNames }),
    ...(filterExpression === undefined ? {} : { FilterExpression: filterExpression }),
    ...(indexName === undefined ? {} : { IndexName: indexName }),
    ...(limit === undefined ? {} : { Limit: limit }),
    ...(select === undefined ? {} : { Select: select }),
  };
}

function parseWriteSpec(value: unknown, index: number): WriteSpec {
  const item = asRecord(value, `TransactItems[${index}]`);
  const put = item.Put;
  const deleteValue = item.Delete;
  if ((put === undefined) === (deleteValue === undefined)) {
    throw validationError(`TransactItems[${index}] must contain exactly one Put or Delete`);
  }
  if (put !== undefined) {
    const operation = asRecord(put, `TransactItems[${index}].Put`);
    const storedItem = requiredItem(operation, "Item");
    return {
      ...parseCondition(operation),
      item: storedItem,
      key: storedItem,
      returnOldOnFailure:
        optionalString(operation, "ReturnValuesOnConditionCheckFailure") === "ALL_OLD",
      tableName: requiredString(operation, "TableName"),
      type: "put",
    };
  }
  const operation = asRecord(deleteValue, `TransactItems[${index}].Delete`);
  return {
    ...parseCondition(operation),
    key: requiredItem(operation, "Key"),
    returnOldOnFailure:
      optionalString(operation, "ReturnValuesOnConditionCheckFailure") === "ALL_OLD",
    tableName: requiredString(operation, "TableName"),
    type: "delete",
  };
}

function parseCondition(request: Readonly<Record<string, unknown>>): ConditionSpec {
  const expression = optionalString(request, "ConditionExpression");
  const names = optionalStringMap(request, "ExpressionAttributeNames");
  const values = optionalItem(request, "ExpressionAttributeValues");
  return {
    ...(expression === undefined ? {} : { expression }),
    ...(names === undefined ? {} : { names }),
    ...(values === undefined ? {} : { values }),
  };
}

function parseRequestBody(body: ArrayBuffer): Readonly<Record<string, unknown>> {
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder().decode(body));
  } catch {
    throw validationError("Request body is not valid JSON");
  }
  return asRecord(value, "request");
}

function requiredItem(
  record: Readonly<Record<string, unknown>>,
  name: string,
): DynamoItem {
  const value = record[name];
  if (value === undefined) {
    throw validationError(`Missing required field: ${name}`);
  }
  return asItem(value, name);
}

function optionalItem(
  record: Readonly<Record<string, unknown>>,
  name: string,
): DynamoItem | undefined {
  const value = record[name];
  return value === undefined ? undefined : asItem(value, name);
}

function asItem(value: unknown, path: string): DynamoItem {
  const record = asRecord(value, path);
  const item: Record<string, AttributeValue> = {};
  for (const [name, attribute] of Object.entries(record)) {
    item[name] = asAttributeValue(attribute, `${path}.${name}`);
  }
  return item;
}

function asAttributeValue(value: unknown, path: string): AttributeValue {
  const record = asRecord(value, path);
  const entries = Object.entries(record);
  if (entries.length !== 1) {
    throw validationError(`${path} must contain exactly one DynamoDB attribute value`);
  }
  const entry = entries[0];
  if (entry === undefined) {
    throw validationError(`${path} is empty`);
  }
  const [kind, raw] = entry;
  if (kind === "B" || kind === "N" || kind === "S") {
    if (typeof raw !== "string") {
      throw validationError(`${path}.${kind} must be a string`);
    }
    return { [kind]: raw } as AttributeValue;
  }
  if (kind === "BOOL" || kind === "NULL") {
    if (typeof raw !== "boolean") {
      throw validationError(`${path}.${kind} must be a boolean`);
    }
    return { [kind]: raw } as AttributeValue;
  }
  if (kind === "BS" || kind === "NS" || kind === "SS") {
    if (!Array.isArray(raw) || !raw.every((element) => typeof element === "string")) {
      throw validationError(`${path}.${kind} must be a string array`);
    }
    if (kind === "BS") {
      return { BS: raw };
    }
    if (kind === "NS") {
      return { NS: raw };
    }
    return { SS: raw };
  }
  if (kind === "L") {
    if (!Array.isArray(raw)) {
      throw validationError(`${path}.L must be an array`);
    }
    return { L: raw.map((element, index) => asAttributeValue(element, `${path}.L[${index}]`)) };
  }
  if (kind === "M") {
    return { M: asItem(raw, `${path}.M`) };
  }
  throw validationError(`${path} uses unsupported DynamoDB attribute type: ${kind}`);
}

function requiredString(record: Readonly<Record<string, unknown>>, name: string): string {
  const value = record[name];
  if (typeof value !== "string" || value.length === 0) {
    throw validationError(`${name} must be a non-empty string`);
  }
  return value;
}

function optionalString(
  record: Readonly<Record<string, unknown>>,
  name: string,
): string | undefined {
  const value = record[name];
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "string") {
    throw validationError(`${name} must be a string`);
  }
  return value;
}

function optionalBoolean(
  record: Readonly<Record<string, unknown>>,
  name: string,
): boolean | undefined {
  const value = record[name];
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "boolean") {
    throw validationError(`${name} must be a boolean`);
  }
  return value;
}

function optionalNumber(
  record: Readonly<Record<string, unknown>>,
  name: string,
): number | undefined {
  const value = record[name];
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    throw validationError(`${name} must be an integer`);
  }
  return value;
}

function optionalStringMap(
  record: Readonly<Record<string, unknown>>,
  name: string,
): Readonly<Record<string, string>> | undefined {
  const value = record[name];
  if (value === undefined) {
    return undefined;
  }
  const map = asRecord(value, name);
  if (!Object.values(map).every((entry) => typeof entry === "string")) {
    throw validationError(`${name} must contain only string values`);
  }
  return map as Readonly<Record<string, string>>;
}

function requiredArray(
  record: Readonly<Record<string, unknown>>,
  name: string,
): readonly unknown[] {
  const value = record[name];
  if (!Array.isArray(value)) {
    throw validationError(`${name} must be an array`);
  }
  return value;
}

function requiredRecord(
  record: Readonly<Record<string, unknown>>,
  name: string,
): Readonly<Record<string, unknown>> {
  const value = record[name];
  if (value === undefined) {
    throw validationError(`Missing required field: ${name}`);
  }
  return asRecord(value, name);
}

function asRecord(value: unknown, path: string): Readonly<Record<string, unknown>> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw validationError(`${path} must be an object`);
  }
  return value as Readonly<Record<string, unknown>>;
}
