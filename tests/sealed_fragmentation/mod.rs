//! Transparent external TCP proxy. It never reads keys or changes application
//! modules; the signed handshake and both real services remain on the normal path.
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[derive(Clone, Copy)]
pub enum Fault {
    Pass,
    StopUpstream,
    StopDownstream,
    Slow,
    UpstreamSlow,
    PartialSilence,
    BetweenSilence,
    PartialEof,
    BetweenEof,
    Mac,
    Header,
}
pub struct Proxy {
    _runtime: tokio::runtime::Runtime,
    address: SocketAddr,
    pub selected: Arc<AtomicBool>,
    pub tag_held: Arc<AtomicBool>,
    pub release: Arc<AtomicBool>,
    pub attaches: Arc<AtomicUsize>,
    pub received_records: Arc<AtomicUsize>,
    pub prefix_plaintext: Arc<AtomicUsize>,
}
impl Proxy {
    pub fn start(member: &crate::members::TestMember, fault: Fault) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let address = listener.local_addr().unwrap();
        let selected = Arc::new(AtomicBool::new(false));
        let tag_held = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let attaches = Arc::new(AtomicUsize::new(0));
        let received_records = Arc::new(AtomicUsize::new(0));
        let prefix_plaintext = Arc::new(AtomicUsize::new(0));
        let state = (
            selected.clone(),
            tag_held.clone(),
            release.clone(),
            attaches.clone(),
            received_records.clone(),
            prefix_plaintext.clone(),
        );
        let port = member.port;
        runtime.spawn(async move {
            while let Ok((caller, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(90),
                        relay(caller, port, fault, state),
                    )
                    .await;
                });
            }
        });
        Self {
            _runtime: runtime,
            address,
            selected,
            tag_held,
            release,
            attaches,
            received_records,
            prefix_plaintext,
        }
    }
    pub fn enroll(&self, master: &crate::members::TestMember, member: &crate::members::TestMember) {
        let mut advertised = member.as_member();
        advertised.endpoints = vec![self.address];
        master.ok(&[
            "fleet",
            "enroll",
            &elsecall::member_code::encode(&advertised),
        ]);
    }
}
type State = (
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
);
async fn headers(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
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
    port: u16,
    fault: Fault,
    state: State,
) -> std::io::Result<()> {
    if matches!(fault, Fault::StopUpstream) {
        use std::os::fd::AsRawFd;
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    caller.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
    }
    let request = headers(&mut caller).await?;
    let run = request.starts_with(b"POST /peer/v1/placements/r-");
    let mut member = TcpStream::connect(("127.0.0.1", port)).await?;
    if matches!(fault, Fault::StopDownstream) {
        use std::os::fd::AsRawFd;
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    member.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
    }
    member.write_all(&request).await?;
    if !run {
        tokio::io::copy_bidirectional(&mut caller, &mut member).await?;
        return Ok(());
    }
    state.3.fetch_add(1, Ordering::SeqCst);
    let length = std::str::from_utf8(&request)
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    assert!(length <= 4096);
    let mut body = vec![0; length];
    caller.read_exact(&mut body).await?;
    member.write_all(&body).await?;
    let response = headers(&mut member).await?;
    caller.write_all(&response).await?;
    if !response.starts_with(b"HTTP/1.1 101 ") {
        tokio::io::copy_bidirectional(&mut caller, &mut member).await?;
        return Ok(());
    }
    let (mut cr, mut cw) = caller.split();
    let (mut mr, mut mw) = member.split();
    let up = async {
        if matches!(fault, Fault::StopUpstream) {
            let mut chunk = [0; 4096];
            loop {
                if state.2.load(Ordering::SeqCst) {
                    // Hold one bounded ciphertext buffer before refusing more
                    // caller reads. Dripping it keeps the real member's receive
                    // clock alive while the caller's send is genuinely blocked.
                    let mut buffered = [0; 65536];
                    cr.read_exact(&mut buffered).await?;
                    state.0.store(true, Ordering::SeqCst);
                    for bytes in buffered.chunks(1024) {
                        mw.write_all(bytes).await?;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    std::future::pending::<()>().await;
                }
                let n = cr.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                mw.write_all(&chunk[..n]).await?;
            }
        } else if matches!(fault, Fault::UpstreamSlow) {
            fragment(&mut cr, &mut mw, fault, &state).await?;
        } else {
            tokio::io::copy(&mut cr, &mut mw).await?;
        }
        mw.shutdown().await
    };
    let down = async {
        if matches!(fault, Fault::UpstreamSlow) {
            tokio::io::copy(&mut mr, &mut cw).await?;
        } else {
            fragment(&mut mr, &mut cw, fault, &state).await?;
        }
        cw.shutdown().await
    };
    tokio::try_join!(up, down)?;
    Ok(())
}
// Read-ahead is explicitly bounded. Positive receive-progress tests must not
// accidentally stop the original sender while shaping an entire record. Four
// MiB accommodates the run's bounded output windows plus heartbeat records.
async fn fragment(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    fault: Fault,
    state: &State,
) -> std::io::Result<()> {
    if matches!(fault, Fault::StopDownstream) {
        return shape(reader, writer, fault, state).await;
    }
    let (mut filling, mut buffered) = tokio::io::duplex(4 * 1024 * 1024);
    let pump = async {
        tokio::io::copy(reader, &mut filling).await?;
        filling.shutdown().await
    };
    let shaped = shape(&mut buffered, writer, fault, state);
    tokio::pin!(pump, shaped);
    tokio::select! {
        result = &mut shaped => result,
        result = &mut pump => { result?; shaped.await }
    }
}

async fn shape(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    fault: Fault,
    state: &State,
) -> std::io::Result<()> {
    let mut forwarded_plaintext = 0;
    loop {
        let length = match reader.read_u32().await {
            Ok(n) => n as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        assert!(length > 0 && length <= 65536);
        let selected = length == 65536 && !state.0.swap(true, Ordering::SeqCst);
        if selected {
            state.5.store(forwarded_plaintext, Ordering::SeqCst);
        }
        if selected && matches!(fault, Fault::Slow | Fault::UpstreamSlow) {
            writer.write_u32(length as u32).await?;
            let mut chunk = [0; 4096];
            for _ in 0..16 {
                reader.read_exact(&mut chunk).await?;
                writer.write_all(&chunk).await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            state.1.store(true, Ordering::SeqCst);
            while !state.2.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let mut tag = [0; 16];
            reader.read_exact(&mut tag).await?;
            writer.write_all(&tag).await?;
            forwarded_plaintext += length;
            continue;
        }
        let mut sealed = vec![0; length + 16];
        reader.read_exact(&mut sealed).await?;
        state.4.fetch_add(1, Ordering::SeqCst);
        if selected {
            if matches!(fault, Fault::StopDownstream) {
                // A bounded prefix keeps CLI receives progressing while this
                // proxy refuses more member reads. Upstream heartbeats continue.
                writer.write_u32(length as u32).await?;
                for bytes in sealed[..length].chunks(1024) {
                    writer.write_all(bytes).await?;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                std::future::pending::<()>().await;
            }
            if matches!(fault, Fault::BetweenEof) {
                return Ok(());
            }
            if matches!(fault, Fault::BetweenSilence) {
                std::future::pending::<()>().await;
            }
            writer
                .write_u32(if matches!(fault, Fault::Header) {
                    65537
                } else {
                    length as u32
                })
                .await?;
            if matches!(fault, Fault::PartialEof | Fault::PartialSilence) {
                writer.write_all(&sealed[..4096]).await?;
                if matches!(fault, Fault::PartialEof) {
                    return Ok(());
                }
                std::future::pending::<()>().await;
            }
            if matches!(fault, Fault::Mac) {
                sealed[length + 15] ^= 1;
            }
            writer.write_all(&sealed).await?;
        } else {
            writer.write_u32(length as u32).await?;
            writer.write_all(&sealed).await?;
        }
        forwarded_plaintext += length;
    }
}
