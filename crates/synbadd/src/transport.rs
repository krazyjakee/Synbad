//! Race a bounded set of discovered addresses within one connect budget.

use std::io;
use std::time::Duration;
use synbad_ipc::DiscoveredPeer;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

/// Prefer one dual-stack listener; fall back on systems without IPv6.
/// IPv6-only LANs must be able to reach the addresses discovery publishes.
pub async fn bind_listener(port: u16) -> io::Result<TcpListener> {
    let dual_stack = (|| {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        socket.set_only_v6(false)?;
        #[cfg(unix)]
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&std::net::SocketAddr::from(([0u16; 8], port)).into())?;
        socket.listen(128)?;
        TcpListener::from_std(socket.into())
    })();
    match dual_stack {
        Ok(listener) => Ok(listener),
        Err(e) => {
            tracing::debug!(?e, port, "dual-stack bind unavailable; trying IPv4");
            TcpListener::bind(("0.0.0.0", port)).await
        }
    }
}

pub async fn connect_peer(
    peer: &DiscoveredPeer,
    port: u16,
    budget: Duration,
) -> io::Result<TcpStream> {
    let mut hosts = vec![peer.host.clone()];
    for host in &peer.addresses {
        if !hosts.contains(host) && hosts.len() < 16 {
            hosts.push(host.clone());
        }
    }
    connect_hosts(hosts, port, budget).await
}

async fn connect_hosts(hosts: Vec<String>, port: u16, budget: Duration) -> io::Result<TcpStream> {
    tokio::time::timeout(budget, async {
        let mut attempts = JoinSet::new();
        for host in hosts {
            attempts.spawn(async move { TcpStream::connect((host.as_str(), port)).await });
        }
        let mut last_error = io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "peer has no reachable address",
        );
        while let Some(result) = attempts.join_next().await {
            match result {
                Ok(Ok(stream)) => {
                    stream.set_nodelay(true)?;
                    return Ok(stream); // JoinSet aborts losing attempts.
                }
                Ok(Err(error)) => last_error = error,
                Err(error) => last_error = io::Error::other(error),
            }
        }
        Err(last_error)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "peer connection timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn listener_accepts_ipv4_and_ipv6_when_available() {
        let supports_v6 = TcpListener::bind(("::1", 0)).await.is_ok();
        let listener = bind_listener(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ipv4 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        listener.accept().await.unwrap();
        drop(ipv4);
        if supports_v6 {
            let ipv6 = TcpStream::connect(("::1", port)).await.unwrap();
            listener.accept().await.unwrap();
            drop(ipv6);
        }
    }

    #[tokio::test]
    async fn unreachable_preferred_address_does_not_hide_reachable_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stream = connect_hosts(
            vec!["127.0.0.2".into(), "127.0.0.1".into()],
            port,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), listener.local_addr().unwrap());
        listener.accept().await.unwrap();
    }

    #[tokio::test]
    async fn unavailable_peer_returns_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(
            connect_hosts(vec!["127.0.0.1".into()], port, Duration::from_secs(1))
                .await
                .is_err()
        );
    }
}
