// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

use std::net::IpAddr;
use std::net::SocketAddr;

use anyhow::Result;
use anyhow::anyhow;
use tokio::net::lookup_host;

/// Resolves an endpoint host and validates Lore's signed port configuration.
///
/// Accepting hostnames is required for special bind targets such as Fly.io's
/// `fly-global-services`; parsing IP literals first keeps normal local binds deterministic.
pub(crate) async fn resolve_socket_address(host: &str, port: i32) -> Result<SocketAddr> {
    let port = u16::try_from(port).map_err(|_range_error| anyhow!("invalid port {port}"))?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }

    lookup_host((host, port))
        .await
        .map_err(|error| anyhow!("failed to resolve endpoint host '{host}': {error}"))?
        .next()
        .ok_or_else(|| anyhow!("endpoint host '{host}' resolved to no addresses"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_ip_literals_and_hostnames() {
        assert_eq!(
            resolve_socket_address("127.0.0.1", 41337).await.unwrap(),
            "127.0.0.1:41337".parse().unwrap()
        );
        assert_eq!(
            resolve_socket_address("::", 41337).await.unwrap(),
            "[::]:41337".parse().unwrap()
        );
        assert_eq!(
            resolve_socket_address("localhost", 41337)
                .await
                .unwrap()
                .port(),
            41337
        );
    }

    #[tokio::test]
    async fn rejects_ports_outside_the_socket_range() {
        assert!(resolve_socket_address("127.0.0.1", -1).await.is_err());
        assert!(resolve_socket_address("127.0.0.1", 65_536).await.is_err());
    }
}
