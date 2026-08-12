// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import type {
  AttributeValue,
  CancellationReason,
  ConditionSpec,
  DynamoItem,
  QueryRequest,
  ScalarIndexValue,
  StoredItem,
  StoredKey,
  TableKeySchema,
  WriteSpec,
} from "./dynamo-types";
import {
  ConditionalCheckFailed,
  RequestError,
  TransactionCanceled,
  validationError,
} from "./errors";

const DEFAULT_QUERY_PAGE_SIZE = 500;
const MAX_QUERY_PAGE_SIZE = 500;
const MAX_TRANSACTION_ITEMS = 100;
const MAX_ATTRIBUTES_PER_INSERT = 12;

interface TableNames {
  readonly fragments: string;
  readonly locks: string;
  readonly metadata: string;
  readonly mutable: string;
}

interface QueryTerm {
  readonly attribute: string;
  readonly first: ScalarIndexValue;
  readonly operation: "begins_with" | "between" | "equals";
  readonly second?: ScalarIndexValue;
}

interface SqlFragment {
  readonly parameters: readonly string[];
  readonly sql: string;
}

interface StoredRow {
  readonly item_json: string;
  readonly partition_key: string;
  readonly sort_key: string;
}

export interface QueryResult {
  readonly count: number;
  readonly items?: readonly DynamoItem[];
  readonly lastEvaluatedKey?: DynamoItem;
}

export class LoreD1Repository {
  public constructor(
    private readonly db: D1Database,
    private readonly tableNames: TableNames,
  ) {}

  public hasTable(tableName: string): boolean {
    return this.trySchema(tableName) !== undefined;
  }

  public async get(tableName: string, keyItem: DynamoItem): Promise<DynamoItem | undefined> {
    const key = keyFromItem(this.schema(tableName), keyItem);
    return (await this.getStored(tableName, key))?.item;
  }

  public async batchGet(
    tableName: string,
    keys: readonly DynamoItem[],
  ): Promise<readonly DynamoItem[]> {
    this.schema(tableName);
    if (keys.length === 0) {
      return [];
    }
    if (keys.length > 100) {
      throw validationError("BatchGetItem supports at most 100 keys per table");
    }

    const statements = keys.map((keyItem) => {
      const key = keyFromItem(this.schema(tableName), keyItem);
      return this.db
        .prepare(
          "SELECT partition_key, sort_key, item_json FROM lore_items " +
            "WHERE table_name = ? AND partition_key = ? AND sort_key = ?",
        )
        .bind(tableName, key.partitionKey, key.sortKey);
    });
    const results = await this.db.batch<StoredRow>(statements);
    return results.flatMap((result) =>
      result.results.map((row) => parseStoredItem(row).item),
    );
  }

  public async put(
    tableName: string,
    item: DynamoItem,
    condition: ConditionSpec = {},
  ): Promise<void> {
    const schema = this.schema(tableName);
    const key = keyFromItem(schema, item);
    const statements = this.buildGuardStatements(tableName, key, condition);
    statements.push(...this.buildPutStatements(tableName, key, item));
    statements.push(this.clearGuardStatement());

    try {
      await this.db.batch(statements);
    } catch (error) {
      if (condition.expression !== undefined) {
        const oldItem = await this.get(tableName, keyItemFromItem(schema, item));
        if (!conditionMatches(oldItem, condition)) {
          throw new ConditionalCheckFailed(oldItem);
        }
      }
      throw error;
    }
  }

  public async delete(tableName: string, keyItem: DynamoItem): Promise<void> {
    const key = keyFromItem(this.schema(tableName), keyItem);
    await this.db.batch(this.buildDeleteStatements(tableName, key));
  }

  public async query(request: QueryRequest): Promise<QueryResult> {
    const schema = this.schema(request.TableName);
    if (request.FilterExpression !== undefined) {
      throw validationError("FilterExpression is not supported by the Lore D1 gateway");
    }
    const terms = parseQueryTerms(request);
    const conditions = terms.map((term) => queryTermSql(term));
    const parameters = conditions.flatMap((condition) => condition.parameters);
    let paginationSql = "";
    if (request.ExclusiveStartKey !== undefined) {
      const start = keyFromItem(schema, request.ExclusiveStartKey);
      paginationSql =
        " AND (i.partition_key > ? OR (i.partition_key = ? AND i.sort_key > ?))";
      parameters.push(start.partitionKey, start.partitionKey, start.sortKey);
    }

    const whereSql = conditions.map((condition) => condition.sql).join(" AND ");
    const baseSql =
      " FROM lore_items i WHERE i.table_name = ?" +
      (whereSql.length === 0 ? "" : ` AND ${whereSql}`) +
      paginationSql;
    parameters.unshift(request.TableName);

    if (request.Select === "COUNT") {
      const row = await this.db
        .prepare(`SELECT COUNT(*) AS count${baseSql}`)
        .bind(...parameters)
        .first<{ count: number }>();
      return { count: row?.count ?? 0 };
    }

    const pageSize = Math.min(
      Math.max(request.Limit ?? DEFAULT_QUERY_PAGE_SIZE, 1),
      MAX_QUERY_PAGE_SIZE,
    );
    const result = await this.db
      .prepare(
        `SELECT i.partition_key, i.sort_key, i.item_json${baseSql} ` +
          "ORDER BY i.partition_key, i.sort_key LIMIT ?",
      )
      .bind(...parameters, pageSize + 1)
      .all<StoredRow>();
    const storedItems = result.results.map(parseStoredItem);
    const hasNextPage = storedItems.length > pageSize;
    const page = hasNextPage ? storedItems.slice(0, pageSize) : storedItems;
    const last = page.at(-1);

    return {
      count: page.length,
      items: page.map((stored) => stored.item),
      ...(hasNextPage && last !== undefined
        ? { lastEvaluatedKey: keyItemFromItem(schema, last.item) }
        : {}),
    };
  }

  public async transactWrite(writes: readonly WriteSpec[]): Promise<void> {
    if (writes.length === 0 || writes.length > MAX_TRANSACTION_ITEMS) {
      throw validationError(
        `TransactWriteItems requires between 1 and ${MAX_TRANSACTION_ITEMS} operations`,
      );
    }

    const seenKeys = new Set<string>();
    const statements: D1PreparedStatement[] = [];
    for (const write of writes) {
      const schema = this.schema(write.tableName);
      const key = keyFromItem(schema, write.key);
      const identity = `${write.tableName}\u0000${key.partitionKey}\u0000${key.sortKey}`;
      if (seenKeys.has(identity)) {
        throw validationError("Transaction cannot include multiple operations on one item");
      }
      seenKeys.add(identity);

      statements.push(...this.buildGuardStatements(write.tableName, key, write));
      statements.push(
        ...(write.type === "put" && write.item !== undefined
          ? this.buildPutStatements(write.tableName, key, write.item)
          : this.buildDeleteStatements(write.tableName, key)),
      );
    }
    statements.push(this.clearGuardStatement());

    try {
      await this.db.batch(statements);
    } catch (error) {
      const reasons = await this.cancellationReasons(writes);
      if (reasons.some((reason) => reason.Code === "ConditionalCheckFailed")) {
        throw new TransactionCanceled(reasons);
      }
      throw error;
    }
  }

  private schema(tableName: string): TableKeySchema {
    const schema = this.trySchema(tableName);
    if (schema === undefined) {
      throw new RequestError(
        400,
        "ResourceNotFoundException",
        `Requested resource not found: Table: ${tableName} not found`,
      );
    }
    return schema;
  }

  private trySchema(tableName: string): TableKeySchema | undefined {
    if (tableName === this.tableNames.fragments) {
      return { partitionAttribute: "hash", sortAttribute: "repository_context" };
    }
    if (tableName === this.tableNames.metadata) {
      return { partitionAttribute: "hash" };
    }
    if (tableName === this.tableNames.mutable) {
      return { partitionAttribute: "repository_id", sortAttribute: "key" };
    }
    if (tableName === this.tableNames.locks) {
      return { partitionAttribute: "hash", sortAttribute: "repositoryBranch" };
    }
    return undefined;
  }

  private async getStored(
    tableName: string,
    key: StoredKey,
  ): Promise<StoredItem | undefined> {
    const row = await this.db
      .prepare(
        "SELECT partition_key, sort_key, item_json FROM lore_items " +
          "WHERE table_name = ? AND partition_key = ? AND sort_key = ?",
      )
      .bind(tableName, key.partitionKey, key.sortKey)
      .first<StoredRow>();
    return row === null ? undefined : parseStoredItem(row);
  }

  private buildGuardStatements(
    tableName: string,
    key: StoredKey,
    condition: ConditionSpec,
  ): D1PreparedStatement[] {
    if (condition.expression === undefined) {
      return [];
    }
    const fragment = conditionSql(tableName, key, condition);
    return [
      this.db
        .prepare(
          "INSERT INTO lore_condition_guard (id, value) " +
            `VALUES (1, CASE WHEN ${fragment.sql} THEN 1 ELSE 0 END) ` +
            "ON CONFLICT(id) DO UPDATE SET value = excluded.value",
        )
        .bind(...fragment.parameters),
    ];
  }

  private buildPutStatements(
    tableName: string,
    key: StoredKey,
    item: DynamoItem,
  ): D1PreparedStatement[] {
    const statements = [
      this.db
        .prepare(
          "INSERT INTO lore_items (table_name, partition_key, sort_key, item_json) " +
            "VALUES (?, ?, ?, ?) ON CONFLICT(table_name, partition_key, sort_key) " +
            "DO UPDATE SET item_json = excluded.item_json",
        )
        .bind(tableName, key.partitionKey, key.sortKey, JSON.stringify(item)),
      this.db
        .prepare(
          "DELETE FROM lore_item_attributes " +
            "WHERE table_name = ? AND partition_key = ? AND sort_key = ?",
        )
        .bind(tableName, key.partitionKey, key.sortKey),
    ];

    const attributes = Object.entries(item).flatMap(([name, value]) => {
      const scalar = tryScalarIndexValue(value);
      return scalar === undefined ? [] : [{ name, scalar }];
    });
    for (let offset = 0; offset < attributes.length; offset += MAX_ATTRIBUTES_PER_INSERT) {
      const chunk = attributes.slice(offset, offset + MAX_ATTRIBUTES_PER_INSERT);
      const placeholders = chunk.map(() => "(?, ?, ?, ?, ?, ?)").join(", ");
      const parameters = chunk.flatMap(({ name, scalar }) => [
        tableName,
        key.partitionKey,
        key.sortKey,
        name,
        scalar.kind,
        scalar.value,
      ]);
      statements.push(
        this.db
          .prepare(
            "INSERT INTO lore_item_attributes " +
              "(table_name, partition_key, sort_key, attribute_name, attribute_kind, attribute_value) " +
              `VALUES ${placeholders}`,
          )
          .bind(...parameters),
      );
    }
    return statements;
  }

  private buildDeleteStatements(
    tableName: string,
    key: StoredKey,
  ): D1PreparedStatement[] {
    return [
      this.db
        .prepare(
          "DELETE FROM lore_item_attributes " +
            "WHERE table_name = ? AND partition_key = ? AND sort_key = ?",
        )
        .bind(tableName, key.partitionKey, key.sortKey),
      this.db
        .prepare(
          "DELETE FROM lore_items WHERE table_name = ? AND partition_key = ? AND sort_key = ?",
        )
        .bind(tableName, key.partitionKey, key.sortKey),
    ];
  }

  private clearGuardStatement(): D1PreparedStatement {
    return this.db.prepare("DELETE FROM lore_condition_guard WHERE id = 1");
  }

  private async cancellationReasons(
    writes: readonly WriteSpec[],
  ): Promise<readonly CancellationReason[]> {
    const reasons: CancellationReason[] = [];
    for (const write of writes) {
      const oldItem = await this.get(write.tableName, write.key);
      if (!conditionMatches(oldItem, write)) {
        reasons.push({
          Code: "ConditionalCheckFailed",
          Message: "The conditional request failed",
          ...(write.returnOldOnFailure && oldItem !== undefined ? { Item: oldItem } : {}),
        });
      } else {
        reasons.push({ Code: "None" });
      }
    }
    return reasons;
  }
}

export function repositoryFromEnv(env: Env): LoreD1Repository {
  return new LoreD1Repository(env.DB, {
    fragments: env.FRAGMENTS_TABLE,
    locks: env.LOCKS_TABLE,
    metadata: env.METADATA_TABLE,
    mutable: env.MUTABLE_TABLE,
  });
}

function keyFromItem(schema: TableKeySchema, item: DynamoItem): StoredKey {
  const partitionValue = item[schema.partitionAttribute];
  if (partitionValue === undefined) {
    throw validationError(`Missing partition key attribute: ${schema.partitionAttribute}`);
  }
  const partitionKey = encodedScalar(partitionValue);
  if (schema.sortAttribute === undefined) {
    return { partitionKey, sortKey: "" };
  }
  const sortValue = item[schema.sortAttribute];
  if (sortValue === undefined) {
    throw validationError(`Missing sort key attribute: ${schema.sortAttribute}`);
  }
  const sortKey = encodedScalar(sortValue);
  return { partitionKey, sortKey };
}

function keyItemFromItem(schema: TableKeySchema, item: DynamoItem): DynamoItem {
  const partitionValue = item[schema.partitionAttribute];
  if (partitionValue === undefined) {
    throw validationError(`Stored item is missing partition key: ${schema.partitionAttribute}`);
  }
  const key: Record<string, AttributeValue> = {
    [schema.partitionAttribute]: partitionValue,
  };
  if (schema.sortAttribute !== undefined) {
    const sortValue = item[schema.sortAttribute];
    if (sortValue === undefined) {
      throw validationError(`Stored item is missing sort key: ${schema.sortAttribute}`);
    }
    key[schema.sortAttribute] = sortValue;
  }
  return key;
}

function encodedScalar(value: AttributeValue): string {
  const scalar = scalarIndexValue(value);
  return `${scalar.kind}:${scalar.value}`;
}

function scalarIndexValue(value: AttributeValue): ScalarIndexValue {
  const scalar = tryScalarIndexValue(value);
  if (scalar === undefined) {
    throw validationError("DynamoDB key and expression values must be scalar B, N, or S values");
  }
  return scalar;
}

function tryScalarIndexValue(value: AttributeValue): ScalarIndexValue | undefined {
  if ("B" in value) {
    return { kind: "B", value: base64ToHex(value.B) };
  }
  if ("N" in value) {
    return { kind: "N", value: value.N };
  }
  if ("S" in value) {
    return { kind: "S", value: value.S };
  }
  return undefined;
}

function base64ToHex(value: string): string {
  let binary: string;
  try {
    binary = atob(value);
  } catch {
    throw validationError("Invalid base64 binary attribute value");
  }
  return [...binary]
    .map((character) => character.charCodeAt(0).toString(16).padStart(2, "0"))
    .join("");
}

function conditionSql(
  tableName: string,
  key: StoredKey,
  condition: ConditionSpec,
): SqlFragment {
  const expression = condition.expression;
  if (expression === undefined) {
    return { sql: "1", parameters: [] };
  }
  const terms = expression.split(/\s+AND\s+/i).map((term) => term.trim());
  const fragments = terms.map((term) => {
    const missing = term.match(/^attribute_not_exists\(([^)]+)\)$/i);
    if (missing !== null) {
      const token = missing[1];
      if (token === undefined) {
        throw validationError(`Invalid condition expression: ${term}`);
      }
      const attribute = resolveName(token.trim(), condition.names);
      return attributeExistsSql(tableName, key, attribute, undefined, true);
    }

    const equality = term.match(/^(\S+)\s*=\s*(\S+)$/);
    if (equality === null) {
      throw validationError(`Unsupported condition expression: ${term}`);
    }
    const [, nameToken, valueToken] = equality;
    if (nameToken === undefined || valueToken === undefined) {
      throw validationError(`Invalid condition expression: ${term}`);
    }
    const attribute = resolveName(nameToken, condition.names);
    const value = condition.values?.[valueToken];
    if (value === undefined) {
      throw validationError(`Missing condition expression value: ${valueToken}`);
    }
    return attributeExistsSql(tableName, key, attribute, scalarIndexValue(value), false);
  });
  return {
    sql: fragments.map((fragment) => fragment.sql).join(" AND "),
    parameters: fragments.flatMap((fragment) => fragment.parameters),
  };
}

function attributeExistsSql(
  tableName: string,
  key: StoredKey,
  attribute: string,
  value: ScalarIndexValue | undefined,
  negate: boolean,
): SqlFragment {
  const valueSql = value === undefined ? "" : " AND a.attribute_kind = ? AND a.attribute_value = ?";
  return {
    sql:
      `${negate ? "NOT " : ""}EXISTS (` +
      "SELECT 1 FROM lore_item_attributes a " +
      "WHERE a.table_name = ? AND a.partition_key = ? AND a.sort_key = ? " +
      `AND a.attribute_name = ?${valueSql})`,
    parameters: [
      tableName,
      key.partitionKey,
      key.sortKey,
      attribute,
      ...(value === undefined ? [] : [value.kind, value.value]),
    ],
  };
}

function conditionMatches(item: DynamoItem | undefined, condition: ConditionSpec): boolean {
  if (condition.expression === undefined) {
    return true;
  }
  return condition.expression.split(/\s+AND\s+/i).every((rawTerm) => {
    const term = rawTerm.trim();
    const missing = term.match(/^attribute_not_exists\(([^)]+)\)$/i);
    if (missing !== null) {
      const token = missing[1];
      if (token === undefined) {
        return false;
      }
      return item?.[resolveName(token.trim(), condition.names)] === undefined;
    }
    const equality = term.match(/^(\S+)\s*=\s*(\S+)$/);
    if (equality === null) {
      return false;
    }
    const [, nameToken, valueToken] = equality;
    if (nameToken === undefined || valueToken === undefined) {
      return false;
    }
    const actual = item?.[resolveName(nameToken, condition.names)];
    const expected = condition.values?.[valueToken];
    if (actual === undefined || expected === undefined) {
      return false;
    }
    return encodedScalar(actual) === encodedScalar(expected);
  });
}

function parseQueryTerms(request: QueryRequest): readonly QueryTerm[] {
  const expression = request.KeyConditionExpression.trim();
  const match = expression.match(
    /^(\S+)\s*=\s*(\S+)(?:\s+AND\s+(?:begins_with\((\S+),\s*(\S+)\)|(\S+)\s*=\s*(\S+)|(\S+)\s+BETWEEN\s+(\S+)\s+AND\s+(\S+)))?$/i,
  );
  if (match === null) {
    throw validationError(`Unsupported key condition expression: ${expression}`);
  }
  const [, firstName, firstValue, prefixName, prefixValue, equalName, equalValue, betweenName, betweenStart, betweenEnd] = match;
  if (firstName === undefined || firstValue === undefined) {
    throw validationError(`Invalid key condition expression: ${expression}`);
  }

  const terms: QueryTerm[] = [
    queryTerm(request, firstName, firstValue, "equals"),
  ];
  if (prefixName !== undefined && prefixValue !== undefined) {
    terms.push(queryTerm(request, prefixName, prefixValue, "begins_with"));
  } else if (equalName !== undefined && equalValue !== undefined) {
    terms.push(queryTerm(request, equalName, equalValue, "equals"));
  } else if (
    betweenName !== undefined &&
    betweenStart !== undefined &&
    betweenEnd !== undefined
  ) {
    const start = queryTerm(request, betweenName, betweenStart, "between");
    const endValue = request.ExpressionAttributeValues[betweenEnd];
    if (endValue === undefined) {
      throw validationError(`Missing query expression value: ${betweenEnd}`);
    }
    terms.push({ ...start, second: scalarIndexValue(endValue) });
  }
  return terms;
}

function queryTerm(
  request: QueryRequest,
  nameToken: string,
  valueToken: string,
  operation: QueryTerm["operation"],
): QueryTerm {
  const value = request.ExpressionAttributeValues[valueToken];
  if (value === undefined) {
    throw validationError(`Missing query expression value: ${valueToken}`);
  }
  return {
    attribute: resolveName(nameToken, request.ExpressionAttributeNames),
    first: scalarIndexValue(value),
    operation,
  };
}

function queryTermSql(term: QueryTerm): SqlFragment {
  const common =
    "EXISTS (SELECT 1 FROM lore_item_attributes a " +
    "WHERE a.table_name = i.table_name AND a.partition_key = i.partition_key " +
    "AND a.sort_key = i.sort_key AND a.attribute_name = ? AND a.attribute_kind = ?";
  if (term.operation === "equals") {
    return {
      sql: `${common} AND a.attribute_value = ?)`,
      parameters: [term.attribute, term.first.kind, term.first.value],
    };
  }
  if (term.operation === "begins_with") {
    return {
      sql: `${common} AND substr(a.attribute_value, 1, length(?)) = ?)`,
      parameters: [term.attribute, term.first.kind, term.first.value, term.first.value],
    };
  }
  if (term.second === undefined || term.second.kind !== term.first.kind) {
    throw validationError("BETWEEN query values must use the same scalar type");
  }
  return {
    sql: `${common} AND a.attribute_value BETWEEN ? AND ?)`,
    parameters: [
      term.attribute,
      term.first.kind,
      term.first.value,
      term.second.value,
    ],
  };
}

function resolveName(
  token: string,
  names: Readonly<Record<string, string>> | undefined,
): string {
  if (!token.startsWith("#")) {
    return token;
  }
  const resolved = names?.[token];
  if (resolved === undefined) {
    throw validationError(`Missing expression attribute name: ${token}`);
  }
  return resolved;
}

function parseStoredItem(row: StoredRow): StoredItem {
  let item: unknown;
  try {
    item = JSON.parse(row.item_json);
  } catch (error) {
    throw new RequestError(500, "InternalServerError", "Stored item contains invalid JSON", {
      cause: String(error),
    });
  }
  if (!isRecord(item)) {
    throw new RequestError(500, "InternalServerError", "Stored item is not a DynamoDB item");
  }
  return {
    item: item as DynamoItem,
    partitionKey: row.partition_key,
    sortKey: row.sort_key,
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
