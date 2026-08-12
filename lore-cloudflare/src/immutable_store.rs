// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::error::AddressNotFound;
use lore_base::error::NotAuthenticated;
use lore_base::error::PayloadNotFound;
use lore_base::error::SlowDown;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::Fragment;
use lore_storage::FragmentFlags;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::ImmutableStore;
use lore_storage::Partition;
use lore_storage::StoreError;
use lore_storage::TypedBytes;
use lore_storage::store_types::StoreMatch;
use lore_storage::store_types::StoreObliterateStats;
use lore_storage::store_types::StoreQueryResult;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;

use crate::CloudflareClient;
use crate::CloudflareClientError;

const MAX_BATCH: usize = 256;

#[derive(Clone)]
pub struct CloudflareImmutableStore {
    client: CloudflareClient,
}

impl CloudflareImmutableStore {
    pub fn new(client: CloudflareClient) -> Self {
        Self { client }
    }

    async fn query_inner(
        &self,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<QueryResponse, StoreError> {
        self.client
            .post(
                "/v1/immutable/query",
                &QueryRequest {
                    partition,
                    address: address.into(),
                    match_requested: match_requested.into(),
                },
            )
            .await
            .map_err(store_error)
    }

    async fn associate(&self, partition: Partition, address: Address) -> Result<(), StoreError> {
        let _: OkResponse = self
            .client
            .post(
                "/v1/immutable/associate",
                &AssociationRequest {
                    partition,
                    address: address.into(),
                },
            )
            .await
            .map_err(store_error)?;
        Ok(())
    }
}

#[async_trait]
impl ImmutableStore for CloudflareImmutableStore {
    async fn is_available(self: Arc<Self>, _timeout: Duration) -> bool {
        self.client.health().await.is_ok()
    }

    async fn exist(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreMatch, StoreError> {
        Ok(self
            .query_inner(partition, address, match_requested)
            .await?
            .match_made()?)
    }

    async fn exist_batch(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        match_requested: StoreMatch,
    ) -> Result<Vec<StoreMatch>, StoreError> {
        let mut output = Vec::with_capacity(addresses.len());
        for chunk in addresses.chunks(MAX_BATCH) {
            let response: ExistBatchResponse = self
                .client
                .post(
                    "/v1/immutable/exist-batch",
                    &ExistBatchRequest {
                        partition,
                        addresses: chunk.iter().copied().map(AddressDto::from).collect(),
                        match_requested: match_requested.into(),
                    },
                )
                .await
                .map_err(store_error)?;
            if response.matches.len() != chunk.len() {
                return Err(StoreError::internal(
                    "Cloudflare batch response length mismatch",
                ));
            }
            for value in response.matches {
                output.push(StoreMatch::try_from(value).map_err(StoreError::internal)?);
            }
        }
        Ok(output)
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_requested: StoreMatch,
    ) -> Result<StoreQueryResult, StoreError> {
        let response = self
            .query_inner(partition, address, match_requested)
            .await?;
        let match_made = response.match_made()?;
        let fragment = response.fragment.map(Fragment::from).unwrap_or_default();
        Ok(StoreQueryResult {
            fragment,
            match_made,
        })
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        match_required: StoreMatch,
    ) -> Result<(Fragment, Bytes), StoreError> {
        let response = self.query_inner(partition, address, match_required).await?;
        if response.match_made()? < match_required {
            return Err(AddressNotFound::from(address).into());
        }
        let fragment = response
            .fragment
            .map(Fragment::from)
            .ok_or_else(|| StoreError::internal("matched fragment has no metadata"))?;
        lore_storage::validate_fragment_size(&fragment)?;
        let payload = self
            .client
            .get_payload(&address.hash.to_string())
            .await
            .map_err(|error| payload_error(error, address.hash))?;
        lore_storage::validate_fragment_payload(&fragment, payload.len())?;
        Ok((fragment, payload))
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        lore_storage::validate_fragment_metadata(&fragment)?;
        if let Some(bytes) = payload.as_ref() {
            lore_storage::validate_fragment_payload(&fragment, bytes.len())?;
        }

        let existing = self
            .query_inner(partition, address, StoreMatch::MatchFull)
            .await?;
        match (existing.match_made()?, payload) {
            (StoreMatch::MatchFull, None) if !force => return Ok(()),
            (StoreMatch::MatchPartition, None) if !force => {
                return self.associate(partition, address).await;
            }
            (StoreMatch::MatchHash | StoreMatch::MatchNone, None) => {
                return Err(PayloadNotFound::from(address.hash).into());
            }
            (_, Some(payload)) => {
                // Payload first: a crash may leave an orphan, never a readable association to missing bytes.
                self.client
                    .put_payload(&address.hash.to_string(), payload)
                    .await
                    .map_err(store_error)?;
            }
            (_, None) => {}
        }

        let _: OkResponse = self
            .client
            .post(
                "/v1/immutable/put",
                &PutRequest {
                    partition,
                    address: address.into(),
                    fragment: fragment.into(),
                },
            )
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        let start: ObliterationStart = self
            .client
            .post(
                "/v1/immutable/begin-obliteration",
                &HashRequest { hash: address.hash },
            )
            .await
            .map_err(store_error)?;
        let Some(fragment) = start.fragment.map(Fragment::from) else {
            return if start.status == "not_found" {
                Err(AddressNotFound::from(address).into())
            } else {
                Ok(())
            };
        };
        if start.status != "started" {
            return Ok(());
        }

        if fragment.flags & FragmentFlags::PayloadFragmented != 0 {
            let payload = self
                .client
                .get_payload(&address.hash.to_string())
                .await
                .map_err(|error| payload_error(error, address.hash))?
                .to_aligned::<FragmentReference>();
            let references: Vec<_> = payload.as_type_slice::<FragmentReference>().to_vec();
            for reference in references {
                self.clone()
                    .obliterate(
                        partition,
                        Address {
                            hash: reference.hash,
                            context: address.context,
                        },
                        stats.clone(),
                    )
                    .await?;
            }
        }

        let removal: AssociationRemoval = self
            .client
            .post(
                "/v1/immutable/remove-association",
                &AssociationRequest {
                    partition,
                    address: address.into(),
                },
            )
            .await
            .map_err(store_error)?;
        stats.num_fragments.fetch_add(1, Ordering::Relaxed);
        if removal.remaining_associations > 0 {
            let _: OkResponse = self
                .client
                .post(
                    "/v1/immutable/cancel-obliteration",
                    &CancelObliterationRequest {
                        hash: address.hash,
                        fragment: fragment.into(),
                    },
                )
                .await
                .map_err(store_error)?;
            return Ok(());
        }

        self.client
            .delete_payload(&address.hash.to_string())
            .await
            .map_err(store_error)?;
        stats.num_payloads.fetch_add(1, Ordering::Relaxed);
        let _: OkResponse = self
            .client
            .post(
                "/v1/immutable/finish-obliteration",
                &HashRequest { hash: address.hash },
            )
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        _durable: bool,
    ) -> Result<(), StoreError> {
        let matched = self
            .query_inner(source_partition, source_address, StoreMatch::MatchFull)
            .await?
            .match_made()?;
        if matched != StoreMatch::MatchFull {
            return Err(AddressNotFound::from(source_address).into());
        }
        self.associate(
            destination_partition,
            Address {
                hash: source_address.hash,
                context: destination_context,
            },
        )
        .await
    }

    async fn evict(
        self: Arc<Self>,
        _max_capacity: usize,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        Ok(0)
    }

    async fn compact(
        self: Arc<Self>,
        _max_size: usize,
        _at: Option<usize>,
        _sync_data: bool,
        _sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        Ok(None)
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        None
    }
    async fn compact_stop(self: Arc<Self>) {}
    fn max_query_batch(&self) -> Option<usize> {
        Some(MAX_BATCH)
    }
    async fn flush(self: Arc<Self>, _sync_data: bool) -> Result<(), StoreError> {
        Ok(())
    }
    async fn verify(self: Arc<Self>, _heal: bool) -> Result<(), StoreError> {
        self.client.health().await.map_err(store_error)
    }
}

#[derive(Copy, Clone, Serialize)]
struct AddressDto {
    hash: Hash,
    context: Context,
}

impl From<Address> for AddressDto {
    fn from(value: Address) -> Self {
        Self {
            hash: value.hash,
            context: value.context,
        }
    }
}

#[derive(Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FragmentDto {
    flags: u32,
    size_payload: u32,
    size_content: u64,
}

impl From<Fragment> for FragmentDto {
    fn from(value: Fragment) -> Self {
        Self {
            flags: value.flags,
            size_payload: value.size_payload,
            size_content: value.size_content,
        }
    }
}

impl From<FragmentDto> for Fragment {
    fn from(value: FragmentDto) -> Self {
        Self {
            flags: value.flags,
            size_payload: value.size_payload,
            size_content: value.size_content,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QueryRequest {
    partition: Partition,
    address: AddressDto,
    match_requested: u8,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryResponse {
    match_made: u8,
    fragment: Option<FragmentDto>,
}

impl QueryResponse {
    fn match_made(&self) -> Result<StoreMatch, StoreError> {
        StoreMatch::try_from(self.match_made).map_err(StoreError::internal)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExistBatchRequest {
    partition: Partition,
    addresses: Vec<AddressDto>,
    match_requested: u8,
}

#[derive(Deserialize)]
struct ExistBatchResponse {
    matches: Vec<u8>,
}

#[derive(Serialize)]
struct AssociationRequest {
    partition: Partition,
    address: AddressDto,
}

#[derive(Serialize)]
struct PutRequest {
    partition: Partition,
    address: AddressDto,
    fragment: FragmentDto,
}

#[derive(Serialize)]
struct HashRequest {
    hash: Hash,
}

#[derive(Serialize)]
struct CancelObliterationRequest {
    hash: Hash,
    fragment: FragmentDto,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssociationRemoval {
    remaining_associations: usize,
}

#[derive(Deserialize)]
struct ObliterationStart {
    status: String,
    fragment: Option<FragmentDto>,
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
        _ => StoreError::internal_with_context(error, "Cloudflare backend operation failed"),
    }
}

fn payload_error(error: CloudflareClientError, hash: Hash) -> StoreError {
    if matches!(&error, CloudflareClientError::Response { status, .. } if *status == StatusCode::NOT_FOUND)
    {
        PayloadNotFound::from(hash).into()
    } else {
        store_error(error)
    }
}
