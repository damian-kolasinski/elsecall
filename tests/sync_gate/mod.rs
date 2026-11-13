//! A TCP boundary gate: complete the authenticated HTTP upgrade, then hold one
//! stream's response bytes. Every service and encrypted message remains real.
#![allow(dead_code)]
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

pub struct SyncGate {
    _runtime: tokio::runtime::Runtime,
    address: SocketAddr,
    armed: Arc<AtomicBool>,
    blocked: Arc<AtomicBool>,
    released: Arc<Notify>,
    exchanges: Arc<AtomicUsize>,
    fail_sync_after: Arc<AtomicUsize>,
}
impl SyncGate {
    pub fn start(member: &crate::members::TestMember) -> Self {
        Self::start_for(member, b"POST /peer/v1/config-sync/attach ")
    }

    pub fn start_run(member: &crate::members::TestMember) -> Self {
        Self::start_for(member, b"POST /peer/v1/placements/r-")
    }

    fn start_for(member: &crate::members::TestMember, attach: &'static [u8]) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let address = listener.local_addr().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let blocked = Arc::new(AtomicBool::new(false));
        let released = Arc::new(Notify::new());
        let exchanges = Arc::new(AtomicUsize::new(0));
        let fail_sync_after = Arc::new(AtomicUsize::new(0));
        let failure = fail_sync_after.clone();
        let count = exchanges.clone();
        let target = (member.port, attach);
        let (a, b, r) = (armed.clone(), blocked.clone(), released.clone());
        runtime.spawn(async move {
            while let Ok((caller, _)) = listener.accept().await {
                let (armed, blocked, released) = (a.clone(), b.clone(), r.clone());
                let count = count.clone();
                let failure = failure.clone();
                tokio::spawn(async move {
                    let _ = relay(caller, target, armed, blocked, released, count, failure).await;
                });
            }
        });
        Self {
            _runtime: runtime,
            address,
            armed,
            blocked,
            released,
            exchanges,
            fail_sync_after,
        }
    }
    /// Update the enrolled endpoint, through the normal operator API.
    pub fn enroll(&self, master: &crate::members::TestMember, member: &crate::members::TestMember) {
        let mut advertised = member.as_member();
        advertised.endpoints = vec![self.address];
        master.ok(&[
            "fleet",
            "enroll",
            &elsecall::member_code::encode(&advertised),
        ]);
    }
    pub fn exchanges(&self) -> usize {
        self.exchanges.load(Ordering::SeqCst)
    }
    /// Drop the nth upcoming sync connection at the TCP boundary. This can
    /// allow service validation through, then interrupt its delivery.
    pub fn fail_sync_after(&self, connections: usize) {
        assert!(connections > 0);
        self.fail_sync_after.store(connections, Ordering::SeqCst);
    }
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    pub fn wait(&self) {
        let deadline = Instant::now() + crate::processes::WAIT;
        while !self.blocked.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "sync never reached its upgraded channel"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    pub fn release(&self) {
        self.released.notify_one();
    }
}

async fn headers(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 64 * 1024 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        bytes.push(reader.read_u8().await?);
    }
    Ok(bytes)
}
async fn relay(
    mut caller: TcpStream,
    (target, attach): (u16, &[u8]),
    armed: Arc<AtomicBool>,
    blocked: Arc<AtomicBool>,
    released: Arc<Notify>,
    exchanges: Arc<AtomicUsize>,
    fail_sync_after: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let request = headers(&mut caller).await?;
    if request.starts_with(b"POST /peer/v1/config-sync/attach ")
        && fail_sync_after.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            == Ok(1)
    {
        return Err(std::io::ErrorKind::ConnectionReset.into());
    }
    if request.starts_with(b"POST /peer/v1/config-sync/attach ")
        || request.starts_with(b"GET /peer/v1/config-sync/status ")
    {
        exchanges.fetch_add(1, Ordering::SeqCst);
    }
    let gated = request.starts_with(attach) && armed.swap(false, Ordering::SeqCst);
    let mut member = TcpStream::connect(("127.0.0.1", target)).await?;
    member.write_all(&request).await?;
    let (mut cr, mut cw) = caller.split();
    let (mut mr, mut mw) = member.split();
    let upstream = tokio::io::copy(&mut cr, &mut mw);
    let downstream = async {
        if gated {
            let response = headers(&mut mr).await?;
            cw.write_all(&response).await?;
            assert!(
                response.starts_with(b"HTTP/1.1 101 "),
                "sync upgrade failed"
            );
            // Forward the upgrade but hold the first response ciphertext byte.
            // Dropping the gate here disconnects an attached caller.
            let first = mr.read_u8().await?;
            blocked.store(true, Ordering::SeqCst);
            released.notified().await;
            cw.write_u8(first).await?;
        }
        tokio::io::copy(&mut mr, &mut cw).await
    };
    tokio::try_join!(upstream, downstream)?;
    Ok(())
}
