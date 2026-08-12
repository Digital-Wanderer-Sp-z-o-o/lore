// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::InvalidArguments;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::LockRecoveryAuditCursor;
use lore_base::types::LockRecoveryAuditEntry;
use lore_base::types::LockRecoveryAuditQuery;
use lore_base::types::LockResource;
use lore_proto::LockService;
use lore_proto::lock::AdminLockRequest;
use lore_proto::lock::AdminLockResponse;
use lore_proto::lock::LockRequest;
use lore_proto::lock::LockResponse;
use lore_proto::lock::QueryRequest;
use lore_proto::lock::QueryResponse;
use lore_proto::lock::RecoveryAuditCursor;
use lore_proto::lock::RecoveryAuditEntry;
use lore_proto::lock::RecoveryAuditRequest;
use lore_proto::lock::RecoveryAuditResponse;
use lore_proto::lock::StatusRequest;
use lore_proto::lock::StatusResponse;
use lore_proto::lock::UnlockRequest;
use lore_proto::lock::UnlockResponse;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use lore_revision::lore::RepositoryId;
use lore_revision::notification::NotificationSender;
use lore_telemetry::InstrumentProvider;
use opentelemetry::metrics::Histogram;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::info;
use tracing::warn;

use super::extract_correlation_id;
use super::get_repository;
use super::get_user_id;
use super::is_owner_or_admin;
use super::timeout_grpc;
use crate::grpc::can_admin_lock;
use crate::util::setup_execution;

const STATUS_MAX_RESOURCE_LEN: usize = 100;

#[derive(Clone)]
struct LoreLockServiceInstrumentProvider {}

fn lock_query_from_request(
    repository: RepositoryId,
    request: &QueryRequest,
) -> Result<LockQuery, LockError> {
    match (&request.branch, &request.owner, &request.description) {
        // Repository
        (None, None, None) => Ok(LockQuery::Repository(repository)),
        // RepositoryBranch
        (Some(branch), None, None) => Ok(LockQuery::RepositoryBranch(repository, branch.into())),
        // RepositoryBranchDescription
        (Some(branch), None, Some(description)) => Ok(LockQuery::RepositoryBranchDescription(
            repository,
            branch.into(),
            description.clone(),
        )),
        // OwnerRepository
        (None, Some(owner), None) => Ok(LockQuery::OwnerRepository(owner.clone(), repository)),
        // OwnerRepositoryBranch
        (Some(branch), Some(owner), None) => Ok(LockQuery::OwnerRepositoryBranch(
            owner.clone(),
            repository,
            branch.into(),
        )),
        _ => Err(InvalidArguments {
            reason: "unsupported lock query combination".into(),
        }
        .into()),
    }
}

fn resolve_expected_unlock_owner(
    actor_id: &str,
    requested_owner: Option<&str>,
    can_override_owner: bool,
) -> Result<String, Status> {
    let requested_owner = requested_owner.filter(|owner| !owner.is_empty());
    match requested_owner {
        None => Ok(actor_id.to_owned()),
        Some(owner) if owner == actor_id || can_override_owner => Ok(owner.to_owned()),
        Some(_) => Err(Status::permission_denied(
            "Only a repository administrator can release another owner's lock",
        )),
    }
}

fn handle_lock_error(error: LockError) -> Status {
    match error {
        LockError::LockNotFound(_) => Status::not_found(error.to_string()),
        LockError::LockNotOwned(_) => Status::failed_precondition(error.to_string()),
        LockError::NotSupported(_) => Status::unimplemented(error.to_string()),
        LockError::SlowDown(_) => Status::resource_exhausted(error.to_string()),
        LockError::InvalidArguments(_) => Status::invalid_argument(error.to_string()),
        LockError::Internal(_) => {
            warn!(error = ?error, "LockData operation failed");
            Status::internal(error.to_string())
        }
    }
}

#[derive(Clone)]
pub struct LoreLockService {
    lock_store: Arc<dyn LockStore>,
    notification: Arc<dyn NotificationSender>,
    rpc_timeout: Duration,

    instrument_provider: LoreLockServiceInstrumentProvider,
    locking_histogram: Histogram<u64>,
    status_histogram: Histogram<u64>,
}

impl LoreLockService {
    pub fn new(
        lock_store: Arc<dyn LockStore>,
        notification: Arc<dyn NotificationSender>,
        rpc_timeout: Duration,
    ) -> Self {
        let instrument_provider = LoreLockServiceInstrumentProvider {};

        Self {
            lock_store,
            notification,
            rpc_timeout,
            locking_histogram: instrument_provider.length_histogram(
                "locking.request.resources.length",
                vec![1., 5., 10., 25., 50., 75., 100., 200.],
            ),
            status_histogram: instrument_provider.length_histogram(
                "status.request.resources.length",
                vec![
                    1., 5., 10., 50., 100., 200., 300., 500., 2_500., 5_000., 10_000., 20_000.,
                    40_000., 60_000., 80_000.,
                ],
            ),
            instrument_provider,
        }
    }
}

impl InstrumentProvider for LoreLockServiceInstrumentProvider {
    fn namespace(&self) -> &'static str {
        "urc.lock_service"
    }
}

impl LoreLockService {
    async fn lock_as_user(
        &self,
        repository: RepositoryId,
        resources: Vec<lore_proto::lock::Resource>,
        owner_id: &str,
    ) -> Result<Vec<lore_proto::lock::Lock>, Status> {
        if resources.is_empty() {
            return Err(Status::invalid_argument("At least one resource needed"));
        }

        let lock_resources: Vec<LockResource> = resources.into_iter().map(Into::into).collect();

        let locks = self
            .lock_store
            .lock_resources(owner_id, repository, &lock_resources)
            .await
            .map_err(handle_lock_error)?;

        // TODO: UCS-13626 move branch out of individual resources into the main message
        // All resources are on the same branch and the lock call has to be made with at least 1 resource
        let branch = lock_resources[0].branch;
        let locked_resources: Vec<LockResource> =
            locks.iter().map(|lock| lock.resource.clone()).collect();

        self.notification
            .resource_locked(repository, branch, owner_id, &locked_resources)
            .await;

        let locks = locks.into_iter().map(Into::into).collect();

        Ok(locks)
    }
}

impl LoreLockService {
    async fn handle_lock(
        &self,
        request: Request<LockRequest>,
    ) -> Result<Response<LockResponse>, Status> {
        let repository = get_repository(request.metadata())?;
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let lock_request = request.into_inner();

        self.locking_histogram.record(
            lock_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("lock"),
        );

        if lock_request.resources.is_empty() {
            return Ok(Response::new(LockResponse { locks: vec![] }));
        }

        let resources = lock_request.resources;

        let execution = setup_execution(module_path!(), correlation_id, user_id.clone());

        LORE_CONTEXT
            .scope(execution, async move {
                self.lock_as_user(repository, resources, &user_id)
                    .await
                    .map(|locks| Response::new(LockResponse { locks }))
            })
            .await
    }

    async fn handle_query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let user_id = get_user_id(request.extensions());
        let repository = get_repository(request.metadata())?;
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let query_request = request.get_ref();

        let query =
            lock_query_from_request(repository, query_request).map_err(handle_lock_error)?;

        let execution = setup_execution(module_path!(), correlation_id, user_id.clone());

        LORE_CONTEXT
            .scope(execution, async move {
                self.lock_store
                    .query_locks(query)
                    .await
                    .map(|result| {
                        Response::new(QueryResponse {
                            result: result.into_iter().map(Into::into).collect(),
                        })
                    })
                    .map_err(handle_lock_error)
            })
            .await
    }

    async fn handle_status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let repository = get_repository(request.metadata())?;
        let status_request = request.into_inner();

        if status_request.resources.len() > STATUS_MAX_RESOURCE_LEN {
            return Err(Status::invalid_argument("Resource count exceeds limit"));
        }

        self.status_histogram.record(
            status_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("status"),
        );

        if status_request.resources.is_empty() {
            return Ok(Response::new(StatusResponse { locks: vec![] }));
        }

        info!(
            num_items = status_request.resources.len(),
            "Handling LockService::Status request"
        );

        let resources: Vec<LockResource> = status_request
            .resources
            .into_iter()
            .map(Into::into)
            .collect();

        let execution = setup_execution(module_path!(), correlation_id, user_id.clone());

        LORE_CONTEXT
            .scope(execution, async move {
                let locks = self
                    .lock_store
                    .check_locks_status(repository, &resources)
                    .await
                    .map_err(handle_lock_error)?;

                Ok(Response::new(StatusResponse {
                    locks: locks.into_iter().map(Into::into).collect(),
                }))
            })
            .await
    }

    async fn handle_unlock(
        &self,
        request: Request<UnlockRequest>,
    ) -> Result<Response<UnlockResponse>, Status> {
        let user_id = get_user_id(request.extensions());
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let repository = get_repository(request.metadata())?;
        let can_override_owner = is_owner_or_admin(request.extensions(), repository);
        let unlock_request = request.into_inner();
        let expected_owner = resolve_expected_unlock_owner(
            &user_id,
            unlock_request.expected_owner.as_deref(),
            can_override_owner,
        )?;

        self.locking_histogram.record(
            unlock_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("unlock"),
        );

        if unlock_request.resources.is_empty() {
            return Ok(Response::new(UnlockResponse { resources: vec![] }));
        }

        let resources: Vec<LockResource> =
            unlock_request.resources.iter().map(Into::into).collect();

        let execution = setup_execution(module_path!(), correlation_id, user_id.clone());

        LORE_CONTEXT
            .scope(execution, async move {
                let resources = if expected_owner == user_id {
                    self.lock_store
                        .unlock_resources(
                            user_id.as_str(),
                            expected_owner.as_str(),
                            repository,
                            &resources,
                        )
                        .await
                } else {
                    self.lock_store
                        .recover_resources(
                            user_id.as_str(),
                            expected_owner.as_str(),
                            repository,
                            &resources,
                        )
                        .await
                }
                .map_err(handle_lock_error)?;

                info!(
                    actor_id = %user_id,
                    expected_owner_id = %expected_owner,
                    administrative_override = expected_owner != user_id,
                    resource_count = resources.len(),
                    "Released lock resources with owner compare-and-swap"
                );

                // TODO: UCS-13626 move branch out of individual resources into the main message
                // All resources are on the same branch and the lock call has to be made with at least 1 resource
                if !resources.is_empty() {
                    self.notification
                        .resource_unlocked(repository, resources[0].branch, &user_id, &resources)
                        .await;
                }

                Ok(Response::new(UnlockResponse {
                    resources: resources.into_iter().map(Into::into).collect(),
                }))
            })
            .await
    }

    async fn handle_query_recovery_audit(
        &self,
        request: Request<RecoveryAuditRequest>,
    ) -> Result<Response<RecoveryAuditResponse>, Status> {
        let user_id = get_user_id(request.extensions());
        let repository = get_repository(request.metadata())?;
        if !is_owner_or_admin(request.extensions(), repository) {
            return Err(Status::permission_denied(
                "Only a repository administrator can read lock recovery audit history",
            ));
        }
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let query = recovery_audit_query(request.into_inner())?;
        let execution = setup_execution(module_path!(), correlation_id, user_id);

        LORE_CONTEXT
            .scope(execution, async move {
                let page = self
                    .lock_store
                    .query_recovery_audit(repository, &query)
                    .await
                    .map_err(handle_lock_error)?;
                Ok(Response::new(RecoveryAuditResponse {
                    events: page
                        .entries()
                        .iter()
                        .map(recovery_audit_entry_proto)
                        .collect(),
                    next_cursor: page.next_cursor().map(recovery_audit_cursor_proto),
                }))
            })
            .await
    }

    async fn handle_admin_lock(
        &self,
        request: Request<AdminLockRequest>,
    ) -> Result<Response<AdminLockResponse>, Status> {
        let correlation_id = extract_correlation_id(&request).unwrap_or_default();
        let repository = get_repository(request.metadata())?;
        let extensions = request.extensions().clone();

        let user_id = get_user_id(request.extensions());
        let lock_request = request.into_inner();

        self.locking_histogram.record(
            lock_request.resources.len() as u64,
            &self
                .instrument_provider
                .get_labels_for_operation_context("admin_lock"),
        );

        if lock_request.resources.is_empty() {
            return Ok(Response::new(AdminLockResponse { locks: vec![] }));
        }

        let resources = lock_request.resources;
        let owner = lock_request.owner;

        let execution = setup_execution(module_path!(), correlation_id, user_id.clone());

        LORE_CONTEXT
            .scope(execution, async move {
                if !can_admin_lock(&extensions, repository) {
                    warn!("Attempt to apply admin locks, but user does not have the correct permissions");
                    return Err(Status::permission_denied("Permission denied"));
                }

                self.lock_as_user(repository, resources, &owner)
                    .await
                    .map(|locks| Response::new(AdminLockResponse { locks }))
            })
            .await
    }
}

#[tonic::async_trait]
impl LockService for LoreLockService {
    #[tracing::instrument(name = "LoreLockService::lock", skip_all)]
    async fn lock(&self, request: Request<LockRequest>) -> Result<Response<LockResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_lock(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::query", skip_all)]
    async fn query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_query(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::status", skip_all)]
    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_status(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::unlock", skip_all)]
    async fn unlock(
        &self,
        request: Request<UnlockRequest>,
    ) -> Result<Response<UnlockResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_unlock(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::admin_lock", skip_all)]
    async fn admin_lock(
        &self,
        request: Request<AdminLockRequest>,
    ) -> Result<Response<AdminLockResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_admin_lock(request)).await
    }

    #[tracing::instrument(name = "LoreLockService::query_recovery_audit", skip_all)]
    async fn query_recovery_audit(
        &self,
        request: Request<RecoveryAuditRequest>,
    ) -> Result<Response<RecoveryAuditResponse>, Status> {
        timeout_grpc(self.rpc_timeout, self.handle_query_recovery_audit(request)).await
    }
}

fn recovery_audit_query(request: RecoveryAuditRequest) -> Result<LockRecoveryAuditQuery, Status> {
    let cursor = request
        .cursor
        .map(|cursor| -> Result<LockRecoveryAuditCursor, Status> {
            let event_id = uuid::Uuid::from_slice(&cursor.event_id).map_err(|_uuid_error| {
                Status::invalid_argument("audit cursor event ID must be a UUID")
            })?;
            let recorded_at = timestamp_millis(cursor.recorded_at.as_ref(), "audit cursor")?;
            Ok(LockRecoveryAuditCursor::new(event_id, recorded_at))
        })
        .transpose()?;
    LockRecoveryAuditQuery::try_new(request.limit, cursor)
        .map_err(|error| handle_lock_error(error.into()))
}

fn recovery_audit_entry_proto(entry: &LockRecoveryAuditEntry) -> RecoveryAuditEntry {
    RecoveryAuditEntry {
        event_id: entry.event_id().as_bytes().to_vec().into(),
        actor: entry.actor_id().to_owned(),
        expected_owner: entry.expected_owner_id().to_owned(),
        resources: entry.resources().iter().map(Into::into).collect(),
        recorded_at: Some(timestamp_proto(entry.recorded_at())),
    }
}

fn recovery_audit_cursor_proto(cursor: &LockRecoveryAuditCursor) -> RecoveryAuditCursor {
    RecoveryAuditCursor {
        event_id: cursor.event_id().as_bytes().to_vec().into(),
        recorded_at: Some(timestamp_proto(cursor.recorded_at())),
    }
}

fn timestamp_proto(milliseconds: u64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: (milliseconds / 1_000) as i64,
        nanos: ((milliseconds % 1_000) * 1_000_000) as i32,
    }
}

fn timestamp_millis(
    timestamp: Option<&prost_types::Timestamp>,
    field: &str,
) -> Result<u64, Status> {
    let timestamp = timestamp
        .ok_or_else(|| Status::invalid_argument(format!("{field} timestamp is required")))?;
    if timestamp.seconds < 0 || !(0..1_000_000_000).contains(&timestamp.nanos) {
        return Err(Status::invalid_argument(format!(
            "{field} timestamp is invalid"
        )));
    }
    let seconds = u64::try_from(timestamp.seconds).map_err(|_timestamp_error| {
        Status::invalid_argument(format!("{field} timestamp is invalid"))
    })?;
    seconds
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(timestamp.nanos as u64 / 1_000_000))
        .ok_or_else(|| Status::invalid_argument(format!("{field} timestamp is out of range")))
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::time::Duration;

    use lore_base::types::LockRecoveryAuditEntry;
    use lore_base::types::LockRecoveryAuditPage;
    use lore_base::types::LockResource;
    use lore_proto::LockService;
    use lore_revision::lore::RepositoryId;
    use lore_transport::grpc::REPOSITORY_ID_KEY;
    use rand::random;
    use tonic::Code;
    use tonic::Request;

    use crate::grpc::lock_service::LoreLockService;
    use crate::grpc::lock_service::resolve_expected_unlock_owner;

    fn authorize<T>(
        request: &mut Request<T>,
        repository: RepositoryId,
        user_id: &str,
        permissions: &[&str],
    ) {
        use crate::auth::jwt::AuthorizationToken;
        use crate::auth::jwt::ResourcePermission;

        request.metadata_mut().insert_bin(
            REPOSITORY_ID_KEY,
            tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
        );
        request.extensions_mut().insert(AuthorizationToken {
            user_id: user_id.to_owned(),
            resources: Some(vec![ResourcePermission {
                resource_id: format!("urc-{repository}"),
                permission: permissions
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
            }]),
            ..Default::default()
        });
    }

    mod store {
        use async_trait::async_trait;
        use lore_base::types::LockData;
        use lore_base::types::LockRecoveryAuditPage;
        use lore_base::types::LockRecoveryAuditQuery;
        use lore_base::types::LockResource;
        use lore_revision::lock::LockError;
        use lore_revision::lock::LockQuery;
        use lore_revision::lock::LockStore;
        use lore_revision::lore::RepositoryId;

        mockall::mock! {
             pub MockLockStore {}

             #[async_trait]
             impl LockStore for MockLockStore {

                async fn lock_resources(
                    &self,
                    owner_id: &str,
                    repository: RepositoryId,
                    resources: &[LockResource],
                ) -> Result<Vec<LockData>, LockError>;

                async fn query_locks(&self, query: LockQuery) -> Result<Vec<LockData>, LockError>;

                async fn check_locks_status(
                    &self,
                    repository: RepositoryId,
                    resources: &[LockResource],
                ) -> Result<Vec<LockData>, LockError>;


                async fn unlock_resources(
                    &self,
                    actor_id: &str,
                    expected_owner_id: &str,
                    repository: RepositoryId,
                    resources: &[LockResource],
                ) -> Result<Vec<LockResource>, LockError>;

                async fn recover_resources(
                    &self,
                    actor_id: &str,
                    expected_owner_id: &str,
                    repository: RepositoryId,
                    resources: &[LockResource],
                ) -> Result<Vec<LockResource>, LockError>;

                async fn query_recovery_audit(
                    &self,
                    repository: RepositoryId,
                    query: &LockRecoveryAuditQuery,
                ) -> Result<LockRecoveryAuditPage, LockError>;
            }
        }
    }

    mod ownership {
        use super::*;

        #[test]
        fn defaults_to_the_authenticated_actor() {
            assert_eq!(
                resolve_expected_unlock_owner("artist", None, true).unwrap(),
                "artist"
            );
        }

        #[test]
        fn permits_an_admin_to_compare_a_foreign_owner() {
            assert_eq!(
                resolve_expected_unlock_owner("admin", Some("artist"), true).unwrap(),
                "artist"
            );
        }

        #[test]
        fn rejects_a_foreign_owner_for_an_ordinary_member() {
            let error = resolve_expected_unlock_owner("member", Some("artist"), false).unwrap_err();
            assert_eq!(error.code(), Code::PermissionDenied);
        }
    }

    mod recovery_audit {
        use lore_proto::lock::RecoveryAuditRequest;
        use uuid::Uuid;

        use super::*;
        use crate::notification::local::NotificationSender;

        #[tokio::test]
        async fn ordinary_member_cannot_read_recovery_audit() {
            let lock_store = super::store::MockMockLockStore::new();
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                Arc::new(NotificationSender::default()),
                Duration::from_secs(60),
            );
            let repository = random::<RepositoryId>();
            let mut request = Request::new(RecoveryAuditRequest {
                limit: 25,
                cursor: None,
            });
            authorize(&mut request, repository, "member", &["read"]);

            let error = lock_service
                .query_recovery_audit(request)
                .await
                .expect_err("ordinary members must not read the administrative audit");

            assert_eq!(error.code(), Code::PermissionDenied);
        }

        #[tokio::test]
        async fn repository_admin_receives_typed_recovery_audit_page() {
            let event_id = Uuid::new_v4();
            let resource = LockResource {
                description: "interiors/unit-01/scene.blend".into(),
                ..Default::default()
            };
            let entry = LockRecoveryAuditEntry::try_new(
                event_id,
                "admin".into(),
                "artist".into(),
                vec![resource],
                1_725_000_000_123,
            )
            .unwrap();
            let mut lock_store = super::store::MockMockLockStore::new();
            lock_store
                .expect_query_recovery_audit()
                .withf(|_, query| query.limit() == 25 && query.cursor().is_none())
                .return_once(move |_, _| Ok(LockRecoveryAuditPage::new(vec![entry], None)));
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                Arc::new(NotificationSender::default()),
                Duration::from_secs(60),
            );
            let repository = random::<RepositoryId>();
            let mut request = Request::new(RecoveryAuditRequest {
                limit: 25,
                cursor: None,
            });
            authorize(&mut request, repository, "admin", &["admin"]);

            let response = lock_service
                .query_recovery_audit(request)
                .await
                .expect("repository admin should read the audit")
                .into_inner();

            assert_eq!(response.events.len(), 1);
            assert_eq!(response.events[0].event_id.as_ref(), event_id.as_bytes());
            assert_eq!(response.events[0].actor, "admin");
            assert_eq!(response.events[0].expected_owner, "artist");
            assert_eq!(response.events[0].resources.len(), 1);
            assert!(response.next_cursor.is_none());
        }
    }

    mod status {
        use lore_proto::lock::Resource;
        use lore_proto::lock::StatusRequest;

        use super::*;
        use crate::notification::local::NotificationSender;

        #[tokio::test]
        async fn resource_count_exceeds_limit() {
            let lock_store = super::store::MockMockLockStore::new();

            let notification_sender = Arc::new(NotificationSender::default());
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                notification_sender,
                Duration::from_secs(60),
            );

            let resources: Vec<Resource> = (0..101)
                .map(|_| Resource {
                    branch: Default::default(),
                    hash: Default::default(),
                    description: "".to_string(),
                })
                .collect();

            let mut request = Request::new(StatusRequest { resources });
            let repository = random::<RepositoryId>();
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let error_status = lock_service
                .status(request)
                .await
                .expect_err("Status should fail when resource count exceeds limit");

            assert_eq!(error_status.code(), Code::InvalidArgument);
        }

        #[tokio::test]
        async fn resource_count_at_limit() {
            let mut lock_store = super::store::MockMockLockStore::new();
            lock_store
                .expect_check_locks_status()
                .return_once(|_, _| Ok(vec![]));

            let notification_sender = Arc::new(NotificationSender::default());
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                notification_sender,
                Duration::from_secs(60),
            );

            let resources: Vec<Resource> = (0..100)
                .map(|_| Resource {
                    branch: Default::default(),
                    hash: Default::default(),
                    description: "".to_string(),
                })
                .collect();

            let mut request = Request::new(StatusRequest { resources });
            let repository = random::<RepositoryId>();
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let _ = lock_service
                .status(request)
                .await
                .expect("Status should succeed when resource count is at limit");
        }
    }

    mod unlock {
        use lore_proto::lock::AdminLockRequest;
        use lore_proto::lock::LockRequest;
        use lore_proto::lock::Resource;
        use lore_proto::lock::StatusRequest;
        use lore_proto::lock::UnlockRequest;

        use super::*;
        use crate::notification::local::NotificationSender;

        #[tokio::test]
        async fn lock_zero_resources() {
            let lock_store = super::store::MockMockLockStore::new();

            let notification_sender = Arc::new(NotificationSender::default());
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                notification_sender,
                Duration::from_secs(60),
            );

            let mut request = Request::new(LockRequest { resources: vec![] });
            let repository = random::<RepositoryId>();
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let _ = lock_service
                .lock(request)
                .await
                .expect("LockData did not return ok status");
        }

        #[tokio::test]
        async fn unlock_zero_resources() {
            let lock_store = super::store::MockMockLockStore::new();

            let notification_sender = Arc::new(NotificationSender::default());
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                notification_sender,
                Duration::from_secs(60),
            );

            let mut request = Request::new(UnlockRequest {
                resources: vec![],
                expected_owner: None,
            });
            let repository = random::<RepositoryId>();
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let _ = lock_service
                .unlock(request)
                .await
                .expect("Unlock did not return ok status");
        }

        #[tokio::test]
        async fn status_zero_resources() {
            let lock_store = super::store::MockMockLockStore::new();

            let notification_sender = Arc::new(NotificationSender::default());
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                notification_sender,
                Duration::from_secs(60),
            );

            let mut request = Request::new(StatusRequest { resources: vec![] });
            let repository = random::<RepositoryId>();
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let _ = lock_service
                .status(request)
                .await
                .expect("Status did not return ok status");
        }

        #[tokio::test]
        async fn admin_unlock_zero_resources() {
            let lock_store = super::store::MockMockLockStore::new();

            let notification_sender = Arc::new(NotificationSender::default());
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                notification_sender,
                Duration::from_secs(60),
            );

            let mut request = Request::new(AdminLockRequest {
                resources: vec![],
                owner: "".to_string(),
            });
            let repository = random::<RepositoryId>();
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let _ = lock_service
                .admin_lock(request)
                .await
                .expect("Admin lock did not return ok status");
        }

        #[tokio::test]
        async fn unlock_fails_for_other_owner() {
            let mut lock_store = super::store::MockMockLockStore::new();
            lock_store
                .expect_unlock_resources()
                .return_once(|_, _, _, _| Err(lore_base::error::LockNotOwned.into()));

            let notification_sender = Arc::new(NotificationSender::default());
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                notification_sender,
                Duration::from_secs(60),
            );

            let mut request = Request::new(UnlockRequest {
                resources: vec![Resource {
                    branch: Default::default(),
                    hash: Default::default(),
                    description: "".to_string(),
                }],
                expected_owner: None,
            });
            let repository = random::<RepositoryId>();
            request.metadata_mut().insert_bin(
                REPOSITORY_ID_KEY,
                tonic::metadata::BinaryMetadataValue::from_bytes(repository.data()),
            );

            let error_status = lock_service
                .unlock(request)
                .await
                .expect_err("Unlock did not return error status");

            assert_eq!(error_status.code(), Code::FailedPrecondition);
        }

        #[tokio::test]
        async fn admin_foreign_unlock_uses_atomic_recovery_store_path() {
            let resource = Resource {
                branch: Default::default(),
                hash: Default::default(),
                description: "interiors/unit-01/scene.blend".into(),
            };
            let mut lock_store = super::store::MockMockLockStore::new();
            lock_store.expect_unlock_resources().never();
            lock_store
                .expect_recover_resources()
                .withf(|actor, owner, _, resources| {
                    actor == "admin"
                        && owner == "artist"
                        && resources.len() == 1
                        && resources[0].description == "interiors/unit-01/scene.blend"
                })
                .return_once(|_, _, _, resources| Ok(resources.to_vec()));
            let lock_service = LoreLockService::new(
                Arc::new(lock_store),
                Arc::new(NotificationSender::default()),
                Duration::from_secs(60),
            );
            let repository = random::<RepositoryId>();
            let mut request = Request::new(UnlockRequest {
                resources: vec![resource],
                expected_owner: Some("artist".into()),
            });
            authorize(&mut request, repository, "admin", &["admin"]);

            let response = lock_service
                .unlock(request)
                .await
                .expect("repository admin should recover a foreign lock")
                .into_inner();

            assert_eq!(response.resources.len(), 1);
        }
    }
}
