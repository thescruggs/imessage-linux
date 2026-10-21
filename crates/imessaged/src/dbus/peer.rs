//! Peer-to-peer D-Bus server: the Windows transport (selectable everywhere
//! with `--transport p2p`, which is how the Linux tests exercise it).
//!
//! There is no bus daemon, so each client gets its own D-Bus connection over
//! TCP on 127.0.0.1, `nonce-tcp` style (as dbus-daemon does it on Windows):
//!
//! * At start the daemon takes `imessaged.lock` in the state directory (a
//!   second daemon there fails at once), writes 16 random bytes to
//!   `bus-nonce` (owner-only), listens on an ephemeral 127.0.0.1 port and
//!   writes `nonce-tcp:host=127.0.0.1,port=N,noncefile=...,guid=...` to
//!   `bus-address` (both atomically replaced). Clients read `bus-address`;
//!   zbus' `nonce-tcp:` client sends the nonce file's contents first.
//! * A connection whose first 16 bytes are not the nonce (constant-time
//!   compare) is closed before any D-Bus traffic. Reading the nonce needs
//!   read access to the state directory, i.e. being this user: that is the
//!   authentication. The SASL step after it is ANONYMOUS on both sides,
//!   because zbus' EXTERNAL over TCP on Windows looks up the peer's process
//!   token, which fails for an elevated client of an unelevated daemon.
//! * Every connection exports the same `Daemon1` object; signals go out on
//!   each live connection ([`Peers::emit`]). A client that stops reading
//!   for [`SIGNAL_TIMEOUT`] is disconnected so it cannot stall the others.
//! * A client sees the daemon go away as its connection closing; it
//!   reconnects by reading `bus-address` again (imsg-client `Bus::Peer`).
//!   There are no bus names, so nothing listens for NameOwnerChanged.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use imessage_common::platform::fs as pfs;
use imessage_common::platform::ipc::{self, BUS_ADDRESS_FILE, LOCK_FILE, NONCE_FILE, NONCE_LEN};
use rand::RngCore;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use zbus::connection::socket::{ReadHalf, Split, WriteHalf};
use zbus::connection::{self, AuthMechanism};
use zbus::object_server::InterfaceRef;

use super::{Daemon1, OBJECT_PATH, UNEXPORT_TIMEOUT};
use crate::hub::{DaemonSignal, Hub};

/// Nonce and SASL handshake together must finish within this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A client whose socket does not take a signal within this is dropped.
pub const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

struct Peer {
    conn: zbus::Connection,
    iface: InterfaceRef<Daemon1>,
}

/// The live client connections.
#[derive(Default)]
pub(crate) struct Peers {
    conns: Mutex<HashMap<u64, Arc<Peer>>>,
    next: AtomicU64,
    stopped: AtomicBool,
}

impl Peers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Peer>>> {
        self.conns.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Register a connection; `None` once the server is stopping.
    fn add(&self, conn: zbus::Connection, iface: InterfaceRef<Daemon1>) -> Option<u64> {
        let mut conns = self.lock();
        if self.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        conns.insert(id, Arc::new(Peer { conn, iface }));
        Some(id)
    }

    fn take(&self, id: u64) -> Option<Arc<Peer>> {
        self.lock().remove(&id)
    }

    /// Number of connected clients.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Send `sig` on every connection, one after the other, each bounded
    /// by [`SIGNAL_TIMEOUT`]. A closed connection is skipped (its task
    /// removes it); a stuck one is closed.
    pub(crate) async fn emit(&self, sig: &DaemonSignal) {
        let peers: Vec<(u64, Arc<Peer>)> = self.lock().iter().map(|(id, p)| (*id, p.clone())).collect();
        for (id, p) in peers {
            if p.conn.is_closed() {
                continue;
            }
            match tokio::time::timeout(SIGNAL_TIMEOUT, super::emit_on(&p.iface, sig)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::debug!("p2p: signal not sent to client {id}: {e}"),
                Err(_) => {
                    log::warn!("p2p: client {id} took no signal within {} s; disconnecting it", SIGNAL_TIMEOUT.as_secs());
                    let _ = p.conn.clone().close().await;
                }
            }
        }
    }
}

/// The running peer-to-peer server.
pub struct PeerServer {
    address: String,
    address_file: PathBuf,
    nonce_file: PathBuf,
    peers: Arc<Peers>,
    accept: JoinHandle<()>,
    _lock: File,
}

/// The read half of an accepted TCP stream, offering ANONYMOUS and no peer
/// credentials (see the module docs). Writes use tokio's half unchanged.
#[derive(Debug)]
struct NonceTcpRead(tokio::net::tcp::OwnedReadHalf);

#[async_trait::async_trait]
impl ReadHalf for NonceTcpRead {
    #[cfg(unix)]
    async fn recvmsg(&mut self, buf: &mut [u8]) -> io::Result<(usize, Vec<std::os::fd::OwnedFd>)> {
        ReadHalf::recvmsg(&mut self.0, buf).await
    }

    #[cfg(not(unix))]
    async fn recvmsg(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        ReadHalf::recvmsg(&mut self.0, buf).await
    }

    fn auth_mechanism(&self) -> AuthMechanism {
        AuthMechanism::Anonymous
    }
}

/// Take the state directory's lock (created owner-only).
fn lock_dir(dir: &Path) -> anyhow::Result<File> {
    crate::state::ensure_private_dir(dir).map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
    let lock_path = dir.join(LOCK_FILE);
    pfs::lock_exclusive(&lock_path).map_err(|e| anyhow::anyhow!("cannot lock {}: {e}", lock_path.display()))?.ok_or_else(|| {
        anyhow::anyhow!(
            "another imessaged is already running for {} (it holds {}); stop it first (`imsg service stop`)",
            dir.display(),
            lock_path.display()
        )
    })
}

/// Fail if another daemon serves from `dir` (the lock is released again).
pub fn check_not_running(dir: &Path) -> anyhow::Result<()> {
    lock_dir(dir).map(drop)
}

/// Lock the state directory, write the nonce and address files and start
/// accepting clients.
pub(crate) async fn serve(hub: Arc<Hub>, dir: &Path) -> anyhow::Result<(PeerServer, Arc<Peers>)> {
    let lock = lock_dir(dir)?;

    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let nonce_file = dir.join(NONCE_FILE);
    crate::state::atomic_write(&nonce_file, &nonce).map_err(|e| anyhow::anyhow!("cannot write {}: {e}", nonce_file.display()))?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1: {e}"))?;
    let port = listener.local_addr()?.port();
    let guid = zbus::Guid::generate();
    let address = ipc::nonce_tcp_address(port, &nonce_file, guid.as_str());
    let address_file = dir.join(BUS_ADDRESS_FILE);
    crate::state::atomic_write(&address_file, format!("{address}\n").as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", address_file.display()))?;

    let peers = Arc::new(Peers::default());
    let accept = tokio::spawn(accept_loop(listener, nonce, guid.to_string(), hub, peers.clone()));
    log::info!("D-Bus peer-to-peer: listening on 127.0.0.1:{port} (address in {})", address_file.display());
    Ok((PeerServer { address, address_file, nonce_file, peers: peers.clone(), accept, _lock: lock }, peers))
}

async fn accept_loop(listener: TcpListener, nonce: [u8; NONCE_LEN], guid: String, hub: Arc<Hub>, peers: Arc<Peers>) {
    loop {
        match listener.accept().await {
            Ok((stream, from)) => {
                tokio::spawn(serve_client(stream, from, nonce, guid.clone(), hub.clone(), peers.clone()));
            }
            Err(e) => {
                // Out of descriptors and the like: do not spin.
                log::warn!("p2p: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Check the nonce, run the D-Bus handshake, serve until the client leaves.
async fn serve_client(mut stream: TcpStream, from: std::net::SocketAddr, nonce: [u8; NONCE_LEN], guid: String, hub: Arc<Hub>, peers: Arc<Peers>) {
    let mut got = [0u8; NONCE_LEN];
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut got)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return log::debug!("p2p: {from}: closed before sending the nonce: {e}"),
        Err(_) => return log::debug!("p2p: {from}: no nonce within {} s", HANDSHAKE_TIMEOUT.as_secs()),
    }
    if !ipc::constant_time_eq(&got, &nonce) {
        return log::warn!("p2p: rejected a connection from {from}: wrong nonce");
    }
    let _ = stream.set_nodelay(true);
    let (r, w) = stream.into_split();
    let split = Split::new(Box::new(NonceTcpRead(r)) as Box<dyn ReadHalf>, Box::new(w) as Box<dyn WriteHalf>);
    let build = async {
        connection::Builder::socket(split)
            .server(guid.as_str())?
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .serve_at(OBJECT_PATH, Daemon1 { hub })?
            .build()
            .await
    };
    let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, build).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return log::warn!("p2p: {from}: D-Bus handshake failed: {e}"),
        Err(_) => return log::warn!("p2p: {from}: D-Bus handshake did not finish within {} s", HANDSHAKE_TIMEOUT.as_secs()),
    };
    let iface = match conn.object_server().interface::<_, Daemon1>(OBJECT_PATH).await {
        Ok(i) => i,
        Err(e) => return log::warn!("p2p: {from}: {e}"),
    };
    let Some(id) = peers.add(conn.clone(), iface) else {
        // The daemon is stopping.
        release(conn).await;
        return;
    };
    log::info!("p2p: client {id} connected ({} now)", peers.len());
    conn.closed().await;
    if let Some(p) = peers.take(id) {
        release(p.conn.clone()).await;
        log::info!("p2p: client {id} disconnected ({} left)", peers.len());
    }
}

/// Unexport the object (breaking connection -> Daemon1 -> Hub) and close.
async fn release(conn: zbus::Connection) {
    if tokio::time::timeout(UNEXPORT_TIMEOUT, conn.object_server().remove::<Daemon1, _>(OBJECT_PATH)).await.is_err() {
        log::debug!("p2p: object not unexported within {} s (a method call is still running)", UNEXPORT_TIMEOUT.as_secs());
    }
    let _ = conn.close().await;
}

impl PeerServer {
    /// The address clients connect to (also in `bus-address`).
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Stop accepting, disconnect every client and remove the address and
    /// nonce files (the lock goes with `self`).
    pub(crate) async fn stop(self) {
        self.accept.abort();
        self.peers.stopped.store(true, Ordering::SeqCst);
        let all: Vec<Arc<Peer>> = self.peers.lock().drain().map(|(_, p)| p).collect();
        for p in all {
            release(p.conn.clone()).await;
        }
        // Only our own address: never delete what a later daemon wrote.
        if std::fs::read_to_string(&self.address_file).is_ok_and(|s| s.trim() == self.address) {
            let _ = std::fs::remove_file(&self.address_file);
        }
        let _ = std::fs::remove_file(&self.nonce_file);
        log::info!("D-Bus peer-to-peer: stopped");
    }
}
