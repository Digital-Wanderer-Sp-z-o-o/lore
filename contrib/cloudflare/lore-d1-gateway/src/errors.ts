// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import type { CancellationReason, DynamoItem } from "./dynamo-types";

export class RequestError extends Error {
  public constructor(
    public readonly status: number,
    public readonly errorType: string,
    message: string,
    public readonly details: Readonly<Record<string, unknown>> = {},
  ) {
    super(message);
    this.name = "RequestError";
  }
}

export class ConditionalCheckFailed extends Error {
  public constructor(public readonly oldItem: DynamoItem | undefined) {
    super("The conditional request failed");
    this.name = "ConditionalCheckFailed";
  }
}

export class TransactionCanceled extends Error {
  public constructor(public readonly reasons: readonly CancellationReason[]) {
    super("Transaction cancelled because one or more conditions failed");
    this.name = "TransactionCanceled";
  }
}

export function validationError(message: string): RequestError {
  return new RequestError(400, "ValidationException", message);
}
