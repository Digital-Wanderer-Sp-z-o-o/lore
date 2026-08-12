// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_revision::event;
use lore_revision::interface::LoreString;
use lore_revision::layer;
use lore_revision::layer::LayerError;
use lore_revision::layer::LoreLayerEntryEventData;
use lore_revision::layer::LoreLayerRecoveryEventData;
use lore_revision::repository::RepositoryContext;

pub async fn list(repository: Arc<RepositoryContext>) -> Result<(), LayerError> {
    let listing = layer::listing(repository).await?;

    for layer in listing.layers {
        event::LoreEvent::LayerEntry(LoreLayerEntryEventData {
            target_path: LoreString::from(&layer.target_path),
            source_repository: layer.repository,
            source_path: LoreString::from(&layer.source_path),
            metadata: layer.metadata.as_ref().into(),
            revision: layer.current,
        })
        .send();
    }

    if let Some(recovery) = listing.recovery {
        event::LoreEvent::LayerRecovery(LoreLayerRecoveryEventData {
            target_path: LoreString::from_str(&recovery.layer.target_path),
            source_repository: recovery.layer.repository,
            source_path: LoreString::from_str(&recovery.layer.source_path),
            metadata: recovery.layer.metadata.as_deref().into(),
            revision: recovery.layer.current,
            operation: recovery.operation,
            purge: recovery.purge.into(),
            force: recovery.force.into(),
        })
        .send();
    }

    Ok(())
}
