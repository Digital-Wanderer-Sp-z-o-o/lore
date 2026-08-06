// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::lore::Address;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::repository::RepositoryWriteToken;
use crate::stage::StageStats;
use crate::stage::stage_delete;
use crate::state;
use crate::store::StoreObliterateStats;
use crate::util;
use crate::util::path::RelativePath;

/// Data for the event emitted when file content is obliterated.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileObliterateEventData {
    /// Address of the obliterated content.
    pub address: Address,
    /// Number of fragments removed.
    pub num_fragments: usize,
    /// Number of payloads removed.
    pub num_payloads: usize,
}

#[error_set]
pub enum ObliterateError {
    InvalidArguments,
    InvalidPath,
    InvalidAddress,
    FileNotFound,
    AddressNotFound,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Disconnected,
    Divergent,
    IdenticalMetadata,
    InvalidNodeHierarchy,
    LayerNotFound,
    LinkNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    Maintenance,
    MaxHistorySearchDepth,
    NodeNotFound,
    NoRemote,
    NotALayer,
    NotALink,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NothingStaged,
    NotSupported,
    Oversized,
    PayloadNotFound,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    RevisionNotFound,
    SharedStoreNotFound,
    SlowDown,
    TokenNotFound,
    WriteRequired,
    MissingIdentity,
}

impl EventError for ObliterateError {
    fn translated(&self) -> LoreError {
        match self {
            ObliterateError::InvalidArguments(_)
            | ObliterateError::InvalidPath(_)
            | ObliterateError::InvalidAddress(_) => LoreError::InvalidArguments,
            ObliterateError::FileNotFound(_) => LoreError::FileNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

pub async fn obliterate_file(
    repository: Arc<RepositoryContext>,
    token: &RepositoryWriteToken,
    path: String,
) -> Result<(), ObliterateError> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        .forward::<ObliterateError>("resolving user path")?;

    let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
        .await
        .forward::<ObliterateError>("Failed to deserialize current revision anchor")?;

    let staged_revision = crate::instance::load_staged_revision(&repository)
        .await
        .ok()
        .flatten()
        .unwrap_or(current_revision);

    let state = state::State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<ObliterateError>("Failed to deserialize state")?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_err(|_err| {
            ObliterateError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;

    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: relative_path.to_string(),
        }
        .into());
    }

    let node = state
        .node(repository.clone(), node_link.node)
        .await
        .map_err(|_err| {
            ObliterateError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;

    if !node.is_file() {
        return Err(InvalidPath {
            path: relative_path.to_string(),
        }
        .into());
    }

    obliterate_address(repository.clone(), node.address).await?;

    util::fs::unlink_recursive(relative_path.to_absolute_path(repository.require_path()?))
        .await
        .internal("Failed to unlink filesystem entry")?;

    let stage_stats = Arc::new(StageStats::default());

    stage_delete(
        repository.clone(),
        state.clone(),
        node_link.node,
        NodeFlags::NoFlags,
        stage_stats,
        None, // TODO(vri): UCS-18010 - Implement obliterate for links
    )
    .await
    .forward::<ObliterateError>("Failed to stage deleted path")?;

    let signature = state
        .serialize(repository.clone(), token)
        .await
        .forward::<ObliterateError>("Failed to serialize staged obliterate state")?;
    crate::instance::store_staged_anchor(&repository, signature)
        .await
        .forward::<ObliterateError>("Failed to serialize revision anchor")?;

    Ok(())
}

pub async fn obliterate_address(
    repository: Arc<RepositoryContext>,
    address: Address,
) -> Result<(), ObliterateError> {
    let stats = Arc::new(StoreObliterateStats::default());

    obliterate_remote_if_configured(repository.clone(), address).await?;

    repository
        .immutable_store()
        .obliterate(repository.id, address, stats.clone())
        .await
        .forward::<ObliterateError>(&format!("Failed to obliterate an address: {address}"))?;

    event::LoreEvent::FileObliterate(LoreFileObliterateEventData {
        address,
        num_fragments: stats.num_fragments.load(Ordering::Relaxed),
        num_payloads: stats.num_payloads.load(Ordering::Relaxed),
    })
    .send();

    Ok(())
}

async fn obliterate_remote_if_configured(
    repository: Arc<RepositoryContext>,
    address: Address,
) -> Result<(), ObliterateError> {
    let remote = match repository.remote().await {
        Ok(remote) => remote,
        Err(lore_transport::ProtocolError::NoRemote(_)) => return Ok(()),
        Err(error) => {
            return Err(error).forward::<ObliterateError>(
                "Failed to connect to the remote before local obliteration",
            );
        }
    };

    let admin = remote
        .admin(repository.id)
        .await
        .forward::<ObliterateError>("Failed to authorize remote obliteration")?;
    admin
        .obliterate(address)
        .await
        .forward::<ObliterateError>(&format!("Failed to obliterate a remote address: {address}"))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use lore_base::types::Context;
    use lore_storage::Fragment;
    use lore_storage::ImmutableStore;
    use lore_storage::StoreMatch;

    use super::*;
    use crate::errors::Disconnected;
    use crate::errors::NoRemote;
    use crate::repository::RepositoryContext;
    use crate::repository::RepositoryFormat;

    async fn repository_with_payload(
        remote: Result<Arc<lore_transport::Connection>, lore_transport::ProtocolError>,
    ) -> (
        Arc<RepositoryContext>,
        Arc<dyn ImmutableStore>,
        crate::lore::RepositoryId,
        Address,
        Bytes,
    ) {
        let (immutable_store, mutable_store) = crate::repository::create_client_memory_stores()
            .await
            .expect("create in-memory stores");
        let repository_id = rand::random();
        let payload = Bytes::from_static(b"must survive a failed remote preflight");
        let address = Address {
            hash: lore_storage::hash::hash_slice(&payload),
            context: rand::random::<Context>(),
        };
        immutable_store
            .clone()
            .put(
                repository_id,
                address,
                Fragment {
                    flags: 0,
                    size_payload: payload.len() as u32,
                    size_content: payload.len() as u64,
                },
                Some(payload.clone()),
                false,
            )
            .await
            .expect("seed local payload");
        let repository = Arc::new(RepositoryContext::new(
            None,
            immutable_store.clone(),
            mutable_store,
            repository_id,
            Default::default(),
            remote,
            Arc::default(),
            RepositoryFormat::Lore,
        ));
        (repository, immutable_store, repository_id, address, payload)
    }

    #[tokio::test]
    async fn remote_connection_failure_preserves_local_payload() {
        let (repository, immutable_store, repository_id, address, payload) =
            repository_with_payload(Err(lore_transport::ProtocolError::from(Disconnected))).await;

        let error = obliterate_address(repository, address)
            .await
            .expect_err("remote failure must stop obliteration");

        assert!(matches!(error, ObliterateError::Disconnected(_)));
        let (_, stored_payload) = immutable_store
            .get(repository_id, address, StoreMatch::MatchFull)
            .await
            .expect("local payload must remain readable");
        assert_eq!(stored_payload, payload);
    }

    #[tokio::test]
    async fn explicitly_offline_repository_obliterates_local_payload() {
        let (repository, immutable_store, repository_id, address, _) =
            repository_with_payload(Err(lore_transport::ProtocolError::from(NoRemote))).await;

        let execution = Arc::new(crate::interface::ExecutionContext::default())
            as Arc<dyn std::any::Any + Send + Sync>;
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, obliterate_address(repository, address))
            .await
            .expect("offline repository may obliterate its local payload");

        let error = immutable_store
            .get(repository_id, address, StoreMatch::MatchFull)
            .await
            .expect_err("obliterated payload must no longer be readable");
        assert!(error.is_payload_not_found());
    }
}
