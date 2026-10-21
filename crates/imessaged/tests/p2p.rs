//! The peer-to-peer transport (Windows' default, `BusChoice::Peer` here):
//! nonce check, one connection per client with signals fanned out to all,
//! single instance per state directory, and what a client sees across a
//! daemon restart. Needs no `dbus-daemon`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use imessage_common::platform::ipc::{BUS_ADDRESS_FILE, NONCE_FILE};
use imessaged::config::DaemonConfig;
use imessaged::daemon::{Daemon, RunningDaemon};
use imessaged::dbus::{BUS_NAME, BusChoice, INTERFACE, OBJECT_PATH};
use imessaged::engine::EngineEvent;
use imessaged::engine::mock::MockEngine;
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};
use tokio::io::AsyncWriteExt;
use zbus::connection::AuthMechanism;

async fn start(dir: &Path, mock: Arc<MockEngine>) -> anyhow::Result<RunningDaemon> {
    let store = StoreHandle::new(Store::open(dir, StateKey::from_bytes([3; 32])).unwrap());
    Daemon::with_engine(DaemonConfig::new(dir.into()), mock).with_store(store).with_dbus(BusChoice::Peer(dir.into())).start().await
}

fn address(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(BUS_ADDRESS_FILE)).unwrap().trim().to_string()
}

fn port(address: &str) -> u16 {
    let p = address.split(',').find_map(|kv| kv.strip_prefix("port=")).expect("port in address");
    p.parse().unwrap()
}

/// A client as imsg-client makes it: zbus' nonce-tcp client, p2p, ANONYMOUS.
async fn client(address: &str) -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::address(address)?.p2p().auth_mechanism(AuthMechanism::Anonymous).build().await
}

async fn proxy(conn: &zbus::Connection) -> zbus::Proxy<'static> {
    // The same well-known destination the generated proxy uses: a peer
    // without registered names accepts it.
    zbus::proxy::Builder::new(conn)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .interface(INTERFACE)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_call_and_both_get_signals() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockEngine::new());
    let running = start(dir.path(), mock.clone()).await.unwrap();
    let addr = address(dir.path());
    assert!(addr.starts_with("nonce-tcp:host=127.0.0.1,port="), "{addr}");
    assert_eq!(running.peer_address(), Some(addr.as_str()));
    assert!(running.dbus().is_none());
    assert_eq!(std::fs::read(dir.path().join(NONCE_FILE)).unwrap().len(), 16);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(NONCE_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "nonce file must be owner-only");
    }

    let (a, b) = (client(&addr).await.unwrap(), client(&addr).await.unwrap());
    let (pa, pb) = (proxy(&a).await, proxy(&b).await);
    let v: String = pa.call("Version", &()).await.unwrap();
    assert_eq!(v, env!("CARGO_PKG_VERSION"));
    let state: String = pb.get_property("ConnectionState").await.unwrap();
    assert_eq!(state, "needs_login");

    let mut sa = pa.receive_signal("TwoFactorRequired").await.unwrap();
    let mut sb = pb.receive_signal("TwoFactorRequired").await.unwrap();
    let mut changed = pb.receive_signal("BackgroundError").await.unwrap();
    mock.inject(EngineEvent::TwoFactorRequired);
    for s in [&mut sa, &mut sb] {
        tokio::time::timeout(Duration::from_secs(5), s.next()).await.expect("signal within 5 s").expect("stream open");
    }
    // A client that leaves does not disturb the others.
    drop((sa, pa));
    a.close().await.unwrap();
    mock.inject(EngineEvent::Error("boom".into()));
    let msg = tokio::time::timeout(Duration::from_secs(5), changed.next()).await.expect("signal within 5 s").unwrap();
    assert_eq!(msg.body().deserialize::<String>().unwrap(), "boom");

    running.shutdown().await.unwrap();
    assert!(!dir.path().join(BUS_ADDRESS_FILE).exists(), "address file removed at shutdown");
    assert!(!dir.path().join(NONCE_FILE).exists(), "nonce file removed at shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_nonce_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(dir.path(), Arc::new(MockEngine::new())).await.unwrap();
    let addr = address(dir.path());
    let port = port(&addr);

    // A wrong nonce, then a D-Bus handshake: the daemon has hung up.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(&[0u8; 16]).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(5), zbus::connection::Builder::tcp_stream(s).p2p().auth_mechanism(AuthMechanism::Anonymous).build()).await;
    assert!(matches!(r, Ok(Err(_))), "handshake after a wrong nonce must fail: {r:?}");

    // No nonce at all (a plain tcp: client) fails too.
    let plain = addr.replacen("nonce-tcp:", "tcp:", 1);
    let plain: String = plain.split(',').filter(|kv| !kv.starts_with("noncefile=")).collect::<Vec<_>>().join(",");
    let r = tokio::time::timeout(Duration::from_secs(15), client(&plain)).await;
    assert!(matches!(r, Ok(Err(_))), "a client without the nonce must fail: {r:?}");

    // The right one still works.
    let c = client(&addr).await.unwrap();
    let v: String = proxy(&c).await.call("Version", &()).await.unwrap();
    assert_eq!(v, env!("CARGO_PKG_VERSION"));
    running.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_instance_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(dir.path(), Arc::new(MockEngine::new())).await.unwrap();
    let first = address(dir.path());

    let other = tempfile::tempdir().unwrap();
    let second = Daemon::with_engine(DaemonConfig::new(dir.path().into()), Arc::new(MockEngine::new()))
        .with_store(StoreHandle::new(Store::open(other.path(), StateKey::from_bytes([3; 32])).unwrap()))
        .with_dbus(BusChoice::Peer(dir.path().into()))
        .start()
        .await;
    let err = second.err().expect("second daemon on the same state dir must fail").to_string();
    assert!(err.contains("already running"), "{err}");
    assert!(imessaged::dbus::peer::check_not_running(dir.path()).is_err());
    assert_eq!(address(dir.path()), first, "the failed start left the address alone");

    // A client notices the daemon going away: its streams end.
    let c = client(&first).await.unwrap();
    let p = proxy(&c).await;
    let mut sig = p.receive_signal("TwoFactorRequired").await.unwrap();
    running.shutdown().await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), sig.next()).await.expect("stream ends within 5 s");
    assert!(end.is_none(), "signal stream must end when the daemon stops");
    assert!(imessaged::dbus::peer::check_not_running(dir.path()).is_ok());

    // Restarted: new address (port, guid); the old one is gone.
    let mock = Arc::new(MockEngine::new());
    let running = start(dir.path(), mock.clone()).await.unwrap();
    let again = address(dir.path());
    assert_ne!(again, first);
    let c = client(&again).await.unwrap();
    let p = proxy(&c).await;
    let mut sig = p.receive_signal("TwoFactorRequired").await.unwrap();
    mock.inject(EngineEvent::TwoFactorRequired);
    tokio::time::timeout(Duration::from_secs(5), sig.next()).await.expect("signal within 5 s").expect("stream open");
    running.shutdown().await.unwrap();
}
