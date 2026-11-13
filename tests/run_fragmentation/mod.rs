//! A transparent TCP boundary: signed attach stays real; four full sealed
//! records of a returned-output frame arrive slowly, each within the existing grace.
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub struct RunFragmentation {
    _runtime: tokio::runtime::Runtime,
    address: SocketAddr,
    records: Arc<AtomicUsize>,
}
impl RunFragmentation {
    pub fn start(member: &crate::members::TestMember) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let address = listener.local_addr().unwrap();
        let records = Arc::new(AtomicUsize::new(0));
        let count = records.clone();
        let port = member.port;
        runtime.spawn(async move {
            while let Ok((caller, _)) = listener.accept().await {
                let count = count.clone();
                tokio::spawn(async move {
                    let _ = relay(caller, port, count).await;
                });
            }
        });
        Self {
            _runtime: runtime,
            address,
            records,
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
    pub fn records(&self) -> usize {
        self.records.load(Ordering::SeqCst)
    }
}

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

async fn relay(mut caller: TcpStream, port: u16, count: Arc<AtomicUsize>) -> std::io::Result<()> {
    let request = headers(&mut caller).await?;
    let run = request.starts_with(b"POST /peer/v1/placements/r-");
    let mut member = TcpStream::connect(("127.0.0.1", port)).await?;
    member.write_all(&request).await?;
    if !run {
        tokio::io::copy_bidirectional(&mut caller, &mut member).await?;
        return Ok(());
    }
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
    let upstream = async {
        tokio::io::copy(&mut cr, &mut mw).await?;
        mw.shutdown().await
    };
    let downstream = async {
        fragment(&mut mr, &mut cw, count).await?;
        cw.shutdown().await
    };
    tokio::try_join!(upstream, downstream)?;
    Ok(())
}

async fn fragment(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    count: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let mut remaining = 4;
    loop {
        let length = match reader.read_u32().await {
            Ok(length) => length as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        if length == 0 || length > 65536 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut sealed = vec![0; length + 16];
        reader.read_exact(&mut sealed).await?;
        writer.write_u32(length as u32).await?;
        if length == 65536 && remaining > 0 {
            for chunk in sealed.chunks(1024) {
                writer.write_all(chunk).await?;
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
            count.fetch_add(1, Ordering::SeqCst);
            remaining -= 1;
        } else {
            writer.write_all(&sealed).await?;
        }
    }
}
