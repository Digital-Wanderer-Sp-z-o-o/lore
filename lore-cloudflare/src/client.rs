// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use reqwest::Method;
use reqwest::StatusCode;
use ring::digest;
use ring::hmac;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CloudflareClientError {
    #[error("request serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Cloudflare request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Cloudflare backend returned HTTP {status}: {message}")]
    Response { status: StatusCode, message: String },
    #[error("system clock is before the Unix epoch")]
    InvalidClock,
}

#[derive(Clone)]
pub struct CloudflareClient {
    http: reqwest::Client,
    endpoint: Arc<str>,
    secret: Arc<[u8]>,
}

impl CloudflareClient {
    pub fn new(
        endpoint: impl Into<String>,
        secret: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, CloudflareClientError> {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            http,
            endpoint: Arc::from(endpoint),
            secret: Arc::from(secret.into().into_bytes()),
        })
    }

    pub async fn health(&self) -> Result<(), CloudflareClientError> {
        let response = self
            .http
            .get(format!("{}/health", self.endpoint))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(response_error(response).await)
        }
    }

    pub async fn post<Req, Res>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<Res, CloudflareClientError>
    where
        Req: Serialize + ?Sized,
        Res: DeserializeOwned,
    {
        let body = Bytes::from(serde_json::to_vec(request)?);
        let response = self.signed_request(Method::POST, path, body).await?;
        decode_json(response).await
    }

    pub async fn put_payload(
        &self,
        hash: &str,
        payload: Bytes,
    ) -> Result<(), CloudflareClientError> {
        let path = format!("/v1/payload/{hash}");
        let response = self.signed_request(Method::PUT, &path, payload).await?;
        ensure_success(response).await
    }

    pub async fn get_payload(&self, hash: &str) -> Result<Bytes, CloudflareClientError> {
        let path = format!("/v1/payload/{hash}");
        let response = self
            .signed_request(Method::GET, &path, Bytes::new())
            .await?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        Ok(response.bytes().await?)
    }

    pub async fn delete_payload(&self, hash: &str) -> Result<(), CloudflareClientError> {
        let path = format!("/v1/payload/{hash}");
        let response = self
            .signed_request(Method::DELETE, &path, Bytes::new())
            .await?;
        ensure_success(response).await
    }

    async fn signed_request(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> Result<reqwest::Response, CloudflareClientError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_clock_error| CloudflareClientError::InvalidClock)?
            .as_secs()
            .to_string();
        let body_digest = digest::digest(&digest::SHA256, &body);
        let canonical = format!(
            "{timestamp}\n{}\n{path}\n{}",
            method.as_str(),
            hex::encode(body_digest.as_ref()),
        );
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.secret);
        let signature = hex::encode(hmac::sign(&key, canonical.as_bytes()).as_ref());
        Ok(self
            .http
            .request(method, format!("{}{}", self.endpoint, path))
            .header("content-type", "application/json")
            .header("x-lore-timestamp", timestamp)
            .header("x-lore-signature", signature)
            .body(body)
            .send()
            .await?)
    }
}

async fn decode_json<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, CloudflareClientError> {
    if !response.status().is_success() {
        return Err(response_error(response).await);
    }
    Ok(serde_json::from_slice(&response.bytes().await?)?)
}

async fn ensure_success(response: reqwest::Response) -> Result<(), CloudflareClientError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(response_error(response).await)
    }
}

async fn response_error(response: reqwest::Response) -> CloudflareClientError {
    let status = response.status();
    let message = response
        .text()
        .await
        .unwrap_or_else(|_| "unreadable response".to_string());
    CloudflareClientError::Response { status, message }
}
