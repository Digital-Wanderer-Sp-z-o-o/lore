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
use lore_base::types::ObliterationAuditCursor;
use lore_base::types::ObliterationAuditEntry;
use lore_base::types::ObliterationAuditEntryData;
use lore_base::types::ObliterationAuditPage;
use lore_base::types::ObliterationAuditQuery;
use lore_base::types::ObliterationAuditStatus;
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
        let execution = lore_revision::lore::execution_context();
        let actor = execution.user_id().await;
        if actor.is_empty() {
            return Err(NotAuthenticated.into());
        }
        let correlation_id = match execution.globals().correlation_id.to_string() {
            correlation_id if correlation_id.is_empty() => uuid::Uuid::new_v4().to_string(),
            correlation_id => correlation_id,
        };
        let start: ObliterationStart = self
            .client
            .post(
                "/v1/immutable/begin-audited-obliteration",
                &BeginObliterationRequest {
                    partition,
                    address: address.into(),
                    actor,
                    correlation_id,
                },
            )
            .await
            .map_err(store_error)?;
        match start.status {
            ObliterationStartStatus::NotFound => {
                return Err(AddressNotFound::from(address).into());
            }
            ObliterationStartStatus::AlreadyObliterated => return Ok(()),
            ObliterationStartStatus::Started | ObliterationStartStatus::Resuming => {}
        }
        let fragment = start.fragment.map(Fragment::from).ok_or_else(|| {
            StoreError::internal("Cloudflare obliteration start omitted fragment")
        })?;
        let event_id = start.event_id.ok_or_else(|| {
            StoreError::internal("Cloudflare obliteration start omitted audit event ID")
        })?;
        let stage = start.stage.ok_or_else(|| {
            StoreError::internal("Cloudflare obliteration start omitted recovery stage")
        })?;

        let remaining_associations = if stage == ObliterationStage::AssociationPending {
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

            self.client
                .post::<_, AssociationRemoval>(
                    "/v1/immutable/remove-audited-association",
                    &ObliterationMutationRequest {
                        event_id: event_id.clone(),
                        partition,
                        address: address.into(),
                    },
                )
                .await
                .map_err(store_error)?
                .remaining_associations
        } else {
            start.remaining_associations.ok_or_else(|| {
                StoreError::internal(
                    "Cloudflare resumed obliteration omitted remaining association count",
                )
            })?
        };
        stats.num_fragments.fetch_add(1, Ordering::Relaxed);
        if remaining_associations > 0 {
            let _: OkResponse = self
                .client
                .post(
                    "/v1/immutable/complete-retained-audited-obliteration",
                    &ObliterationMutationRequest {
                        event_id,
                        partition,
                        address: address.into(),
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
                "/v1/immutable/finish-audited-obliteration",
                &ObliterationMutationRequest {
                    event_id,
                    partition,
                    address: address.into(),
                },
            )
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn query_obliteration_audit(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        query: &ObliterationAuditQuery,
    ) -> Result<ObliterationAuditPage, StoreError> {
        let response: ObliterationAuditResponse = self
            .client
            .post(
                "/v1/immutable/obliteration-audit",
                &ObliterationAuditRequest {
                    repository: partition,
                    address: address.into(),
                    limit: query.limit(),
                    cursor: query.cursor().map(ObliterationAuditCursorDto::from),
                },
            )
            .await
            .map_err(store_error)?;
        obliteration_audit_page(partition, address, query.limit(), response)
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

#[derive(Copy, Clone, Serialize, Deserialize)]
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
#[serde(rename_all = "camelCase")]
struct BeginObliterationRequest {
    partition: Partition,
    address: AddressDto,
    actor: String,
    correlation_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObliterationMutationRequest {
    event_id: String,
    partition: Partition,
    address: AddressDto,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssociationRemoval {
    remaining_associations: usize,
}

#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ObliterationStartStatus {
    Started,
    Resuming,
    AlreadyObliterated,
    NotFound,
}

#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ObliterationStage {
    AssociationPending,
    AssociationRemoved,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObliterationStart {
    status: ObliterationStartStatus,
    event_id: Option<String>,
    stage: Option<ObliterationStage>,
    remaining_associations: Option<usize>,
    fragment: Option<FragmentDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObliterationAuditRequest {
    repository: Partition,
    address: AddressDto,
    limit: u32,
    cursor: Option<ObliterationAuditCursorDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObliterationAuditCursorDto {
    event_id: String,
    recorded_at: u64,
}

impl From<&ObliterationAuditCursor> for ObliterationAuditCursorDto {
    fn from(value: &ObliterationAuditCursor) -> Self {
        Self {
            event_id: value.event_id().to_string(),
            recorded_at: value.recorded_at(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObliterationAuditEntryDto {
    event_id: String,
    actor: String,
    correlation_id: String,
    repository: Partition,
    address: AddressDto,
    status: ObliterationAuditStatusDto,
    remaining_associations: Option<u64>,
    recorded_at: u64,
    completed_at: Option<u64>,
}

#[derive(Copy, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ObliterationAuditStatusDto {
    AssociationPending,
    AssociationRemoved,
    PayloadRetained,
    PayloadObliterated,
}

impl From<ObliterationAuditStatusDto> for ObliterationAuditStatus {
    fn from(value: ObliterationAuditStatusDto) -> Self {
        match value {
            ObliterationAuditStatusDto::AssociationPending => Self::AssociationPending,
            ObliterationAuditStatusDto::AssociationRemoved => Self::AssociationRemoved,
            ObliterationAuditStatusDto::PayloadRetained => Self::PayloadRetained,
            ObliterationAuditStatusDto::PayloadObliterated => Self::PayloadObliterated,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObliterationAuditResponse {
    events: Vec<ObliterationAuditEntryDto>,
    next_cursor: Option<ObliterationAuditCursorDto>,
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

fn obliteration_audit_page(
    repository: Partition,
    address: Address,
    requested_limit: u32,
    response: ObliterationAuditResponse,
) -> Result<ObliterationAuditPage, StoreError> {
    if response.events.len() > requested_limit as usize {
        return Err(invalid_obliteration_audit(
            "response exceeded the requested page size",
        ));
    }
    let entries = response
        .events
        .into_iter()
        .map(|event| {
            let event_address = Address {
                hash: event.address.hash,
                context: event.address.context,
            };
            if event.repository != repository || event_address != address {
                return Err(invalid_obliteration_audit(
                    "response contained a different repository or address",
                ));
            }
            let event_id = uuid::Uuid::try_parse(&event.event_id)
                .map_err(|_uuid_error| invalid_obliteration_audit("event ID was not a UUID"))?;
            ObliterationAuditEntry::try_new(ObliterationAuditEntryData {
                event_id,
                actor_id: event.actor,
                correlation_id: event.correlation_id,
                repository: event.repository,
                address: event_address,
                status: event.status.into(),
                remaining_associations: event.remaining_associations,
                recorded_at: event.recorded_at,
                completed_at: event.completed_at,
            })
            .map_err(|error| invalid_obliteration_audit(&error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !entries.windows(2).all(|pair| {
        (pair[0].recorded_at(), pair[0].event_id()) > (pair[1].recorded_at(), pair[1].event_id())
    }) {
        return Err(invalid_obliteration_audit(
            "events were not in stable newest-first order",
        ));
    }
    let next_cursor = response
        .next_cursor
        .map(|cursor| {
            uuid::Uuid::try_parse(&cursor.event_id)
                .map(|event_id| ObliterationAuditCursor::new(event_id, cursor.recorded_at))
                .map_err(|_uuid_error| invalid_obliteration_audit("cursor event ID was not a UUID"))
        })
        .transpose()?;
    if let Some(cursor) = next_cursor.as_ref() {
        let Some(last) = entries.last() else {
            return Err(invalid_obliteration_audit(
                "response included a cursor without events",
            ));
        };
        if cursor.event_id() != last.event_id() || cursor.recorded_at() != last.recorded_at() {
            return Err(invalid_obliteration_audit(
                "cursor did not identify the final page event",
            ));
        }
    }
    Ok(ObliterationAuditPage::new(entries, next_cursor))
}

fn invalid_obliteration_audit(message: &str) -> StoreError {
    StoreError::internal(format!(
        "invalid Cloudflare obliteration audit response: {message}"
    ))
}

#[cfg(test)]
mod obliteration_audit_tests {
    use rand::random;

    use super::*;

    fn completed_event(
        repository: Partition,
        address: Address,
        recorded_at: u64,
    ) -> ObliterationAuditEntryDto {
        ObliterationAuditEntryDto {
            event_id: uuid::Uuid::new_v4().to_string(),
            actor: "owner".into(),
            correlation_id: "correlation".into(),
            repository,
            address: address.into(),
            status: ObliterationAuditStatusDto::PayloadObliterated,
            remaining_associations: Some(0),
            recorded_at,
            completed_at: Some(recorded_at + 1),
        }
    }

    #[test]
    fn accepts_exact_stable_audit_page() {
        let repository = random::<Partition>();
        let address = random::<Address>();
        let event = completed_event(repository, address, 10);
        let cursor = ObliterationAuditCursorDto {
            event_id: event.event_id.clone(),
            recorded_at: event.recorded_at,
        };
        let page = obliteration_audit_page(
            repository,
            address,
            1,
            ObliterationAuditResponse {
                events: vec![event],
                next_cursor: Some(cursor),
            },
        )
        .expect("valid page should pass");
        assert_eq!(page.entries().len(), 1);
        assert!(page.next_cursor().is_some());
    }

    #[test]
    fn rejects_cross_address_or_inconsistent_audit_data() {
        let repository = random::<Partition>();
        let address = random::<Address>();
        let mut wrong_address = completed_event(repository, random::<Address>(), 10);
        assert!(
            obliteration_audit_page(
                repository,
                address,
                1,
                ObliterationAuditResponse {
                    events: vec![wrong_address],
                    next_cursor: None,
                },
            )
            .is_err()
        );

        wrong_address = completed_event(repository, address, 10);
        wrong_address.remaining_associations = Some(1);
        assert!(
            obliteration_audit_page(
                repository,
                address,
                1,
                ObliterationAuditResponse {
                    events: vec![wrong_address],
                    next_cursor: None,
                },
            )
            .is_err()
        );
    }
}
