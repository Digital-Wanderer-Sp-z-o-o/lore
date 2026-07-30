// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

export type ScalarAttributeValue =
  | { readonly B: string }
  | { readonly N: string }
  | { readonly S: string };

export type AttributeValue =
  | ScalarAttributeValue
  | { readonly BOOL: boolean }
  | { readonly BS: readonly string[] }
  | { readonly L: readonly AttributeValue[] }
  | { readonly M: Readonly<Record<string, AttributeValue>> }
  | { readonly NS: readonly string[] }
  | { readonly NULL: boolean }
  | { readonly SS: readonly string[] };

export type DynamoItem = Readonly<Record<string, AttributeValue>>;

export interface DescribeTableRequest {
  readonly TableName: string;
}

export interface GetItemRequest {
  readonly ConsistentRead?: boolean;
  readonly Key: DynamoItem;
  readonly TableName: string;
}

export interface KeysAndAttributes {
  readonly ConsistentRead?: boolean;
  readonly Keys: readonly DynamoItem[];
}

export interface BatchGetItemRequest {
  readonly RequestItems: Readonly<Record<string, KeysAndAttributes>>;
}

export interface PutItemRequest {
  readonly ConditionExpression?: string;
  readonly ExpressionAttributeNames?: Readonly<Record<string, string>>;
  readonly ExpressionAttributeValues?: DynamoItem;
  readonly Item: DynamoItem;
  readonly ReturnValuesOnConditionCheckFailure?: "ALL_OLD" | "NONE";
  readonly TableName: string;
}

export interface DeleteItemRequest {
  readonly Key: DynamoItem;
  readonly TableName: string;
}

export interface QueryRequest {
  readonly ConsistentRead?: boolean;
  readonly ExclusiveStartKey?: DynamoItem;
  readonly ExpressionAttributeNames?: Readonly<Record<string, string>>;
  readonly ExpressionAttributeValues: DynamoItem;
  readonly FilterExpression?: string;
  readonly IndexName?: string;
  readonly KeyConditionExpression: string;
  readonly Limit?: number;
  readonly Select?: "ALL_ATTRIBUTES" | "COUNT";
  readonly TableName: string;
}

export interface TransactionPut {
  readonly ConditionExpression?: string;
  readonly ExpressionAttributeNames?: Readonly<Record<string, string>>;
  readonly ExpressionAttributeValues?: DynamoItem;
  readonly Item: DynamoItem;
  readonly ReturnValuesOnConditionCheckFailure?: "ALL_OLD" | "NONE";
  readonly TableName: string;
}

export interface TransactionDelete {
  readonly ConditionExpression?: string;
  readonly ExpressionAttributeNames?: Readonly<Record<string, string>>;
  readonly ExpressionAttributeValues?: DynamoItem;
  readonly Key: DynamoItem;
  readonly ReturnValuesOnConditionCheckFailure?: "ALL_OLD" | "NONE";
  readonly TableName: string;
}

export type TransactWriteItem =
  | { readonly Put: TransactionPut }
  | { readonly Delete: TransactionDelete };

export interface TransactWriteItemsRequest {
  readonly TransactItems: readonly TransactWriteItem[];
}

export interface TableKeySchema {
  readonly partitionAttribute: string;
  readonly sortAttribute?: string;
}

export interface StoredKey {
  readonly partitionKey: string;
  readonly sortKey: string;
}

export interface StoredItem extends StoredKey {
  readonly item: DynamoItem;
}

export interface ScalarIndexValue {
  readonly kind: "B" | "N" | "S";
  readonly value: string;
}

export interface ConditionSpec {
  readonly expression?: string;
  readonly names?: Readonly<Record<string, string>>;
  readonly values?: DynamoItem;
}

export interface WriteSpec extends ConditionSpec {
  readonly item?: DynamoItem;
  readonly key: DynamoItem;
  readonly returnOldOnFailure: boolean;
  readonly tableName: string;
  readonly type: "delete" | "put";
}

export interface CancellationReason {
  readonly Code: "ConditionalCheckFailed" | "None";
  readonly Item?: DynamoItem;
  readonly Message?: string;
}
