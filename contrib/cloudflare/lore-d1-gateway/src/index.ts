// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import {
  ConditionalCheckFailed,
  RequestError,
  TransactionCanceled,
} from "./errors";
import { handleDynamoRequest } from "./handler";
import { repositoryFromEnv } from "./repository";
import { verifySigV4 } from "./sigv4";

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const requestId = crypto.randomUUID();
    try {
      const url = new URL(request.url);
      if (request.method === "GET" && url.pathname === "/health") {
        return healthResponse(env, requestId);
      }
      if (request.method !== "POST" || url.pathname !== "/") {
        return new Response("Not found", { status: 404 });
      }

      const verified = await verifySigV4(
        request,
        env.AUTH_ACCESS_KEY_ID,
        env.AUTH_SECRET_ACCESS_KEY,
      );
      const responseBody = await handleDynamoRequest(
        verified.target,
        verified.body,
        repositoryFromEnv(env),
      );
      console.log(
        JSON.stringify({
          event: "dynamo_request",
          requestId,
          status: 200,
          target: verified.target,
        }),
      );
      return awsJson(responseBody, 200, requestId);
    } catch (error) {
      return errorResponse(error, requestId);
    }
  },
} satisfies ExportedHandler<Env>;

async function healthResponse(env: Env, requestId: string): Promise<Response> {
  try {
    await env.DB.prepare("SELECT 1 FROM lore_items LIMIT 1").first();
    return Response.json(
      { database: "ready", service: "archigma-lore-d1-gateway" },
      { headers: { "x-request-id": requestId } },
    );
  } catch (error) {
    console.error(JSON.stringify({ event: "health_failed", error: String(error), requestId }));
    return Response.json(
      { database: "unavailable", service: "archigma-lore-d1-gateway" },
      { headers: { "x-request-id": requestId }, status: 503 },
    );
  }
}

function errorResponse(error: unknown, requestId: string): Response {
  if (error instanceof ConditionalCheckFailed) {
    return awsError(
      "ConditionalCheckFailedException",
      error.message,
      400,
      requestId,
      error.oldItem === undefined ? {} : { Item: error.oldItem },
    );
  }
  if (error instanceof TransactionCanceled) {
    return awsError(
      "TransactionCanceledException",
      error.message,
      400,
      requestId,
      { CancellationReasons: error.reasons },
    );
  }
  if (error instanceof RequestError) {
    return awsError(
      error.errorType,
      error.message,
      error.status,
      requestId,
      error.details,
    );
  }

  console.error(JSON.stringify({ event: "unhandled_error", error: String(error), requestId }));
  return awsError(
    "InternalServerError",
    "The Lore metadata gateway encountered an internal error",
    500,
    requestId,
  );
}

function awsError(
  errorType: string,
  message: string,
  status: number,
  requestId: string,
  details: Readonly<Record<string, unknown>> = {},
): Response {
  console.warn(
    JSON.stringify({ errorType, event: "dynamo_error", message, requestId, status }),
  );
  return awsJson(
    {
      __type: `com.amazonaws.dynamodb.v20120810#${errorType}`,
      message,
      ...details,
    },
    status,
    requestId,
    { "x-amzn-errortype": errorType },
  );
}

function awsJson(
  body: unknown,
  status: number,
  requestId: string,
  extraHeaders: Readonly<Record<string, string>> = {},
): Response {
  return new Response(JSON.stringify(body), {
    headers: {
      "content-type": "application/x-amz-json-1.0",
      "x-amzn-requestid": requestId,
      ...extraHeaders,
    },
    status,
  });
}
