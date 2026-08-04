// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use tonic::Request;
use tonic::Status;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::grpc::ServerResultExt;

/// Connect an internal auth/ReBAC client while preserving the advertised auth
/// URL used by Lore clients and token stores.
///
/// Lore advertises protocol implementations with custom schemes such as
/// `ucs-auth://`. Tonic needs a transport scheme, so remote custom schemes are
/// carried over HTTPS. Explicit HTTP remains available for local/test auth
/// services, matching the server's existing behavior.
pub async fn connect_auth_channel(auth_url: &str) -> Result<Channel, Status> {
    let transport_url = auth_transport_url(auth_url)?;
    let mut endpoint = Endpoint::from_shared(transport_url.clone())
        .warn_map_err(|_| Status::internal("Failed to create auth endpoint"))?;
    if transport_url.starts_with("https://") {
        endpoint = endpoint
            .tls_config(
                ClientTlsConfig::new()
                    .assume_http2(true)
                    .with_native_roots(),
            )
            .warn_map_err(|_| Status::internal("Failed to configure TLS for auth endpoint"))?;
    }
    endpoint
        .connect()
        .await
        .warn_map_err(|_| Status::internal("Failed to connect to auth endpoint"))
}

fn auth_transport_url(auth_url: &str) -> Result<String, Status> {
    let auth_url = auth_url.trim();
    if auth_url.is_empty() {
        return Err(Status::internal("Auth endpoint is empty"));
    }
    match auth_url.split_once("://") {
        Some(("http" | "https", _)) => Ok(auth_url.to_string()),
        Some((_, authority_and_path)) if !authority_and_path.is_empty() => {
            Ok(format!("https://{authority_and_path}"))
        }
        Some(_) => Err(Status::internal("Auth endpoint has no authority")),
        None => Ok(format!("https://{auth_url}")),
    }
}

// TODO: if no authorization string is passed, do not add a metadata for 'authorization'.
// See test can_create_request_without_authorization
fn grpc_set_authorization_metadata<F>(
    request: &mut Request<F>,
    authorization: Option<String>,
) -> Result<(), Status> {
    let auth_header: tonic::metadata::MetadataValue<_> = authorization
        .unwrap_or_default()
        .parse()
        .warn_map_err(|err| Status::internal(format!("Failed to create metadata: {err}")))?;
    request.metadata_mut().append("authorization", auth_header);
    Ok(())
}

pub fn create_request_with_authorization<T>(
    payload: T,
    authorization: Option<String>,
) -> Result<Request<T>, Status> {
    let mut request = tonic::Request::new(payload);
    grpc_set_authorization_metadata(&mut request, authorization)?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use anyhow::Error;

    use super::{auth_transport_url, create_request_with_authorization};

    #[test]
    fn custom_auth_scheme_uses_https_transport() {
        assert_eq!(
            auth_transport_url("ucs-auth://auth.example.com/api").unwrap(),
            "https://auth.example.com/api"
        );
    }

    #[test]
    fn explicit_transport_scheme_is_preserved() {
        assert_eq!(
            auth_transport_url("https://auth.example.com").unwrap(),
            "https://auth.example.com"
        );
        assert_eq!(
            auth_transport_url("http://127.0.0.1:3010").unwrap(),
            "http://127.0.0.1:3010"
        );
    }

    #[test]
    fn bare_auth_host_defaults_to_https() {
        assert_eq!(
            auth_transport_url("auth.example.com").unwrap(),
            "https://auth.example.com"
        );
    }

    #[test]
    fn can_create_request_with_authorization() -> Result<(), Error> {
        let payload = (4, 20);
        let request = create_request_with_authorization(payload, Some("my-auth".into()))?;
        assert_eq!(request.get_ref(), &payload);

        let auth_metadata = request.metadata().get("authorization").unwrap();
        assert_eq!(auth_metadata.to_str()?, "my-auth");

        Ok(())
    }

    #[test]
    fn can_create_request_without_authorization() -> Result<(), Error> {
        let payload = (4, 20);
        let request = create_request_with_authorization(payload, None)?;
        assert_eq!(request.get_ref(), &payload);

        let auth_metadata = request.metadata().get("authorization").unwrap();
        // looks dodgy to me but this was the original code.
        // to reduce the surface area we will keep this - providing None to `authorization`
        // results in an empty authorization metadata. If you come across this and think
        // it is strange then you are right and it could probably be changed; something
        // we don't have time/risk to investigate right
        assert_eq!(auth_metadata.to_str()?, "");

        Ok(())
    }
}
