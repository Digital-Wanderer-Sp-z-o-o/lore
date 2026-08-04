// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::LinkError;
use crate::branch;
use crate::event;
use crate::interface::LoreString;
use crate::link::LoreLinkEntryEventData;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_warn;
use crate::node::NodeID;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::state::StateNodeChildrenIterator;

pub async fn list(repository: Arc<RepositoryContext>) -> Result<(), LinkError> {
    let (_state_current, state_staged, parent_branch) =
        State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<LinkError>("Failed deserializing state")?;
    let state_staged = state_staged.unwrap_or_else(|| _state_current.clone());

    lore_debug!("Listing links in repository");

    let link_list = state_staged
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;

    for link_reference in link_list {
        let link_path = state_staged
            .node_path(repository.clone(), link_reference.local_node)
            .await
            .forward::<LinkError>("Failed resolving link node")?;

        let link = Arc::new(repository.to_link_context(link_reference.repository).await);

        let local_node = state_staged
            .node(repository.clone(), link_reference.local_node)
            .await
            .forward::<LinkError>("Specified node is not a link node")?;

        let resolved_branch = link_reference.resolve_branch(parent_branch);
        let details = resolve_link_details(
            link.clone(),
            link_reference.signature,
            local_node.child,
            resolved_branch,
        )
        .await;
        let (source_path, branch_name, content_available) = match details {
            Ok(details) => (details.source_path, details.branch_name, true),
            Err(error) => {
                lore_warn!(
                    "Linked repository {} content is unavailable or restricted; listing mount metadata only: {error}",
                    link_reference.repository,
                );
                (String::new(), String::new(), false)
            }
        };

        event::LoreEvent::LinkEntry(LoreLinkEntryEventData {
            link: link_reference.repository,
            link_node: link_reference.local_node,
            link_path: link_path.into(),
            source_node: local_node.child,
            source_path: source_path.into(),
            branch: resolved_branch,
            branch_name: LoreString::from(branch_name.as_str()),
            revision: link_reference.signature,
            flags: link_reference.flags,
            content_available: content_available.into(),
        })
        .send();
    }

    Ok(())
}

struct LinkDetails {
    source_path: String,
    branch_name: String,
}

async fn resolve_link_details(
    link: Arc<RepositoryContext>,
    revision: crate::lore::Hash,
    source_node: NodeID,
    branch_id: crate::lore::BranchId,
) -> Result<LinkDetails, LinkError> {
    let link_state = State::deserialize(link.clone(), revision)
        .await
        .forward::<LinkError>("Failed deserializing linked repository state")?;
    let source_path = link_state
        .node_path(link.clone(), source_node)
        .await
        .forward::<LinkError>("Failed resolving linked source node")?;
    let source_path = if source_path.is_empty() {
        String::from("/")
    } else {
        source_path
    };
    let branch_name = if let Ok(metadata) = branch::metadata(link, branch_id).await {
        branch::name(&metadata).unwrap_or_default().to_string()
    } else {
        String::new()
    };
    Ok(LinkDetails {
        source_path,
        branch_name,
    })
}

/// Data for an event describing a link that has staged changes.
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLinkStagedEntryEventData {
    /// Path of the link within the parent repository.
    pub path: LoreString,
    /// Identifier of the repository the link points to.
    pub repository: RepositoryId,
    /// Number of staged files inside the link.
    pub staged_file_count: u64,
}

#[derive(Clone, Debug)]
pub struct StagedLinkInfo {
    pub path: String,
    pub repository: RepositoryId,
    pub staged_file_count: u64,
}

pub async fn list_staged(
    repository: Arc<RepositoryContext>,
) -> Result<Vec<StagedLinkInfo>, LinkError> {
    let staged_revision = match crate::instance::load_staged_revision(&repository).await {
        Ok(Some(revision)) => revision,
        Ok(None) | Err(_) => return Ok(Vec::new()),
    };

    let state_staged = State::deserialize(repository.clone(), staged_revision)
        .await
        .forward::<LinkError>("Failed deserializing state")?;

    let link_list = state_staged
        .link_list(repository.clone())
        .await
        .forward::<LinkError>("Failed to list links")?;

    let mut result = Vec::new();
    for link_ref in &link_list {
        let node = state_staged
            .node(repository.clone(), link_ref.local_node)
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;

        if !node.is_staged() {
            continue;
        }

        // Newly added links don't have staged content inside
        if node.is_staged_add() {
            continue;
        }

        let link_path = state_staged
            .node_path(repository.clone(), link_ref.local_node)
            .await
            .forward::<LinkError>("Failed resolving link node")?;

        // Count staged files inside the link
        let link_repository = Arc::new(repository.to_link_context(link_ref.repository).await);
        let link_state = State::deserialize(link_repository.clone(), link_ref.signature)
            .await
            .forward::<LinkError>("Failed deserializing state node block")?;

        let staged_file_count =
            count_staged_files(link_repository.clone(), link_state.clone(), node.child).await;

        let info = StagedLinkInfo {
            path: link_path.clone(),
            repository: link_ref.repository,
            staged_file_count,
        };

        event::LoreEvent::LinkStagedEntry(LoreLinkStagedEntryEventData {
            path: LoreString::from_str(&link_path),
            repository: link_ref.repository,
            staged_file_count,
        })
        .send();

        result.push(info);
    }

    Ok(result)
}

async fn count_staged_files(
    repository: Arc<RepositoryContext>,
    state: Arc<State>,
    node_id: NodeID,
) -> u64 {
    let mut count = 0u64;
    let children =
        match StateNodeChildrenIterator::new(state.clone(), repository.clone(), node_id).await {
            Ok(iter) => iter,
            Err(err) => {
                lore_warn!("Failed to iterate children for staged file count: {err}");
                return 0;
            }
        };

    let mut iter = children;
    while let Ok(Some((child_id, child_node))) = iter.next().await {
        if !child_node.is_staged() {
            continue;
        }
        if child_node.is_file() {
            count += 1;
        } else if child_node.is_directory() {
            count += Box::pin(count_staged_files(
                repository.clone(),
                state.clone(),
                child_id,
            ))
            .await;
        }
    }

    count
}
