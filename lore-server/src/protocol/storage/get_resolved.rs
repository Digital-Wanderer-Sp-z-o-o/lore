// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `get_resolved`: resolve a mutable key and return the immutable blob it points at, in one
//! round trip.
//!
//! Callers that need "look up this name, then fetch what it points at" would otherwise issue
//! `mutable_load` followed by `get` and pay two sequential round trips. This command performs
//! both steps server-side. The resolved hash is echoed back so the caller can cache the
//! key->hash mapping and still verify the payload against it, rather than trusting the
//! server's resolution blindly.
use std::sync::Arc;

use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_base::types::Hash;
use lore_base::types::KeyType;
use lore_revision::lore::RepositoryId;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::StoreMatch;
use tracing::debug;
use tracing::info;
use tracing::warn;
use zerocopy::IntoBytes;

use crate::protocol::storage::messages::LoreResponse;
use crate::protocol::storage::messages::Message;
use crate::protocol::storage::messages::MessageHandleError;
use crate::protocol::storage::messages::MessageParseError;
use crate::protocol::storage::messages::Response;
use crate::util::setup_execution;

/// Bit 0 of the request `flags` byte: also push every referenced subfragment recursively.
///
/// NOT IMPLEMENTED. The QUIC protocol permits exactly one response per `command_id`, so this
/// needs a streaming response first. Parsed and rejected here so that the request layout does
/// not have to change when it is implemented.
pub const FLAG_ALSO_SUBFRAGMENTS: u8 = 1;

/// Wire request: `Hash` key (32) ++ `Context` (16) ++ `key_type` (1) ++ `flags` (1) = 50 bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct GetResolved {
    pub key: Hash,
    /// Paired with the resolved hash to form the `Address` of the immutable read. The mutable
    /// store yields only a content hash, so the caller supplies the context.
    pub context: Context,
    pub key_type: KeyType,
    pub flags: u8,
}

impl GetResolved {
    pub fn parse(bytes: Bytes) -> Result<Self, MessageParseError> {
        const KEY: usize = size_of::<Hash>();
        const CTX: usize = size_of::<Context>();
        if bytes.len() < KEY + CTX + 2 {
            return Err(MessageParseError::InvalidFieldLength);
        }

        let key = Hash::from(&bytes[..KEY]);
        let context = Context::from(&bytes[KEY..KEY + CTX]);
        let key_type = KeyType::try_from(bytes[KEY + CTX])
            .map_err(|_err| MessageParseError::InvalidFieldLength)?;
        let flags = bytes[KEY + CTX + 1];

        Ok(Self {
            key,
            context,
            key_type,
            flags,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_get_resolved(
    key: Hash,
    context: Context,
    key_type: KeyType,
    flags: u8,
    repository: RepositoryId,
    correlation_id: String,
    user_id: String,
    mutable_store: Arc<dyn MutableStore>,
    immutable_store: Arc<dyn ImmutableStore>,
) -> Result<LoreResponse, MessageHandleError> {
    let execution = setup_execution(module_path!(), correlation_id, user_id);

    debug!(
        "Handling get_resolved for key: {} key_type: {:?} in repository: {}",
        key, key_type, repository
    );

    if flags & FLAG_ALSO_SUBFRAGMENTS != 0 {
        // Reject rather than silently ignore: a caller that asked for the subfragment push
        // and got only the root would otherwise wait forever for frames that never come.
        warn!("get_resolved: FLAG_ALSO_SUBFRAGMENTS requested but not implemented");
        return Err(MessageHandleError::NotImplemented);
    }

    LORE_CONTEXT
        .scope(execution, async move {
            // Step 1: resolve the mutable key to an immutable content hash.
            let resolved = match mutable_store.load(repository, key, key_type).await {
                Ok(value) => value,
                Err(StoreError::SlowDown(_)) => return Err(MessageHandleError::SlowDown),
                Err(StoreError::AddressNotFound(_)) => {
                    info!("get_resolved: mutable key not found: {}", key);
                    return Err(MessageHandleError::MutableDataNotFound(key));
                }
                Err(err) => {
                    warn!(error = ?err, "get_resolved: failed to load mutable key: {}", key);
                    return Err(MessageHandleError::StoreFailure);
                }
            };

            // Step 2: read the immutable blob that hash addresses.
            let address = Address {
                hash: resolved,
                context,
            };
            match immutable_store
                .get(repository, address, StoreMatch::MatchFull)
                .await
            {
                Ok((mut fragment, payload)) => {
                    debug!(
                        "get_resolved: key {} -> {} ({} payload / {} content bytes)",
                        key, resolved, fragment.size_payload, fragment.size_content
                    );
                    fragment.flags &= !FragmentFlags::PayloadStored;
                    fragment.flags |= FragmentFlags::PayloadStoredDurable;
                    Ok(LoreResponse::GetResolved(GetResolvedResponse {
                        resolved,
                        fragment,
                        payload,
                    }))
                }
                Err(StoreError::SlowDown(_)) => Err(MessageHandleError::SlowDown),
                Err(StoreError::AddressNotFound(_)) => {
                    // The key resolved but its target is gone — a dangling pointer, not a
                    // missing key. Both map to NotFound on the wire; the log distinguishes.
                    info!(
                        "get_resolved: key {} resolved to {} but no fragment was found",
                        key, resolved
                    );
                    Err(MessageHandleError::FragmentNotFound)
                }
                Err(err) => {
                    warn!(error = ?err, "get_resolved: failed to get fragment for {}", address);
                    Err(MessageHandleError::StoreFailure)
                }
            }
        })
        .await
}

// This command needs BOTH the mutable and the immutable store. The v0 (`urc/0.2`) message
// path hands a handler only one store, so it cannot be expressed there; the defaulted trait
// methods return `NotImplemented`. Real dispatch happens in the v4 path, which has both.
impl Message for GetResolved {}

#[derive(Debug, PartialEq)]
pub struct GetResolvedResponse {
    /// The hash the mutable key resolved to.
    pub resolved: Hash,
    pub fragment: Fragment,
    pub payload: Bytes,
}

impl Response for GetResolvedResponse {
    fn data(&self) -> Vec<Bytes> {
        vec![
            Bytes::copy_from_slice(self.resolved.as_bytes()),
            Bytes::copy_from_slice(self.fragment.as_bytes()),
            self.payload.clone(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use lore_base::runtime::LORE_CONTEXT;
    use rand::random;

    use super::*;
    use crate::store::test_store_create;

    fn request_bytes(key: Hash, context: Context, key_type: KeyType, flags: u8) -> Bytes {
        let mut bytes = bytes::BytesMut::with_capacity(size_of::<Hash>() + size_of::<Context>() + 2);
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(context.as_bytes());
        bytes.extend_from_slice(&[key_type as u8, flags]);
        bytes.freeze()
    }

    #[test]
    fn test_parse() {
        let key = Hash::hash_buffer(b"test-key");
        let context = Context::default();
        let parsed =
            GetResolved::parse(request_bytes(key, context, KeyType::BranchMetadata, 0)).unwrap();
        assert_eq!(parsed.key, key);
        assert_eq!(parsed.context, context);
        assert_eq!(parsed.key_type, KeyType::BranchMetadata);
        assert_eq!(parsed.flags, 0);
    }

    #[test]
    fn test_parse_preserves_flags() {
        let key = Hash::hash_buffer(b"test-key");
        let parsed = GetResolved::parse(request_bytes(
            key,
            Context::default(),
            KeyType::Untyped,
            FLAG_ALSO_SUBFRAGMENTS,
        ))
        .unwrap();
        assert_eq!(parsed.flags, FLAG_ALSO_SUBFRAGMENTS);
    }

    #[test]
    fn test_parse_invalid_length() {
        // One byte short of key + context + key_type + flags.
        let bytes = Bytes::from(vec![0u8; size_of::<Hash>() + size_of::<Context>() + 1]);
        assert_eq!(
            GetResolved::parse(bytes),
            Err(MessageParseError::InvalidFieldLength)
        );
    }

    #[tokio::test]
    async fn test_missing_key_is_mutable_not_found() {
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"missing-key");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let result = LORE_CONTEXT
            .scope(execution, async move {
                handle_get_resolved(
                    key,
                    Context::default(),
                    KeyType::Untyped,
                    0,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await;

        assert!(matches!(
            result,
            Err(MessageHandleError::MutableDataNotFound(_))
        ));
    }

    #[tokio::test]
    async fn test_dangling_pointer_is_fragment_not_found() {
        // Key exists but points at a blob that was never stored.
        let repository = random::<RepositoryId>();
        let key = Hash::hash_buffer(b"dangling-key");
        let value = Hash::hash_buffer(b"never-stored");
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let result = LORE_CONTEXT
            .scope(execution, async move {
                mutable_store
                    .clone()
                    .store(repository, key, value, KeyType::Untyped)
                    .await
                    .unwrap();
                handle_get_resolved(
                    key,
                    Context::default(),
                    KeyType::Untyped,
                    0,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await;

        assert!(matches!(result, Err(MessageHandleError::FragmentNotFound)));
    }

    #[tokio::test]
    async fn test_subfragments_flag_rejected() {
        let repository = random::<RepositoryId>();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        let result = LORE_CONTEXT
            .scope(execution, async move {
                handle_get_resolved(
                    Hash::hash_buffer(b"any-key"),
                    Context::default(),
                    KeyType::Untyped,
                    FLAG_ALSO_SUBFRAGMENTS,
                    repository,
                    String::new(),
                    String::new(),
                    mutable_store,
                    immutable_store,
                )
                .await
            })
            .await;

        assert!(matches!(result, Err(MessageHandleError::NotImplemented)));
    }
}
