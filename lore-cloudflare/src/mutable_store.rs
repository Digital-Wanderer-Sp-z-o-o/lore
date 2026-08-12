// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use async_trait::async_trait;
use lore_base::error::AddressNotFound;
use lore_base::error::NotAuthenticated;
use lore_base::error::SlowDown;
use lore_storage::Address;
use lore_storage::Hash;
use lore_storage::MutableStore;
use lore_storage::Partition;
use lore_storage::StoreError;
use lore_storage::store_types::KeyType;
use lore_storage::store_types::KeyValueStream;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;

use crate::CloudflareClient;
use crate::CloudflareClientError;

pub struct CloudflareMutableStore {
    client: CloudflareClient,
}

impl CloudflareMutableStore {
    pub fn new(client: CloudflareClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl MutableStore for CloudflareMutableStore {
    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let response: LoadResponse = self
            .client
            .post(
                "/v1/mutable/load",
                &KeyRequest {
                    partition,
                    key,
                    key_type: key_type as u8,
                },
            )
            .await
            .map_err(store_error)?;
        response
            .value
            .ok_or_else(|| AddressNotFound::from(Address::zero_context_hash(key)).into())
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        let _: OkResponse = self
            .client
            .post(
                "/v1/mutable/store",
                &StoreRequest {
                    partition,
                    key,
                    value,
                    key_type: key_type as u8,
                },
            )
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        let response: CompareAndSwapResponse = self
            .client
            .post(
                "/v1/mutable/compare-and-swap",
                &CompareAndSwapRequest {
                    partition,
                    key,
                    expected,
                    value,
                    key_type: key_type as u8,
                },
            )
            .await
            .map_err(store_error)?;
        Ok(response.previous)
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        let response: ListResponse = self
            .client
            .post(
                "/v1/mutable/list",
                &ListRequest {
                    partition,
                    key_type: key_type as u8,
                },
            )
            .await
            .map_err(store_error)?;
        let (stream, sender) = KeyValueStream::new();
        for entry in response.entries {
            sender
                .send((entry.key, entry.value))
                .map_err(|_| StoreError::internal("mutable list receiver closed"))?;
        }
        Ok(stream)
    }

    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyRequest {
    partition: Partition,
    key: Hash,
    key_type: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreRequest {
    partition: Partition,
    key: Hash,
    value: Hash,
    key_type: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompareAndSwapRequest {
    partition: Partition,
    key: Hash,
    expected: Hash,
    value: Hash,
    key_type: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListRequest {
    partition: Partition,
    key_type: u8,
}

#[derive(Deserialize)]
struct LoadResponse {
    value: Option<Hash>,
}

#[derive(Deserialize)]
struct CompareAndSwapResponse {
    previous: Hash,
    #[allow(dead_code)]
    swapped: bool,
}

#[derive(Deserialize)]
struct MutableEntry {
    key: Hash,
    value: Hash,
}

#[derive(Deserialize)]
struct ListResponse {
    entries: Vec<MutableEntry>,
}

#[derive(Deserialize)]
struct OkResponse {
    #[allow(dead_code)]
    ok: bool,
}

fn store_error(error: CloudflareClientError) -> StoreError {
    match &error {
        CloudflareClientError::Response { status, .. } if *status == StatusCode::UNAUTHORIZED => {
            NotAuthenticated.into()
        }
        CloudflareClientError::Response { status, .. }
            if status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS =>
        {
            SlowDown.into()
        }
        CloudflareClientError::Transport(_) => SlowDown.into(),
        _ => StoreError::internal_with_context(error, "Cloudflare mutable store operation failed"),
    }
}
