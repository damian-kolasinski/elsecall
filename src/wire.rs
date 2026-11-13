//! Length-prefixed frames over a byte stream: the encoding of placed-run traffic
//! between a caller and the member running its command. Transport security lives
//! in `secure_channel`; what the frames mean lives in `workspace::sync` and
//! `remote_run`.
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest payload one frame carries; bulk data is split across frames.
pub const MAXIMUM_PAYLOAD: usize = 1024 * 1024;
/// How bulk data is chunked.
pub const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Proof of life when nothing else is sent.
    Heartbeat,
    /// A JSON control message.
    Message(Vec<u8>),
    /// A chunk of workspace or output content; an empty chunk ends a blob.
    Content(Vec<u8>),
    Stdin(Vec<u8>),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

impl Frame {
    fn kind(&self) -> u8 {
        match self {
            Frame::Heartbeat => 0,
            Frame::Message(_) => 1,
            Frame::Content(_) => 2,
            Frame::Stdin(_) => 3,
            Frame::Stdout(_) => 4,
            Frame::Stderr(_) => 5,
        }
    }
    /// Whether the frame fits one write; larger data is chunked.
    pub fn fits(&self) -> bool {
        self.payload().len() <= MAXIMUM_PAYLOAD
    }
    fn payload(&self) -> &[u8] {
        match self {
            Frame::Heartbeat => &[],
            Frame::Message(bytes)
            | Frame::Content(bytes)
            | Frame::Stdin(bytes)
            | Frame::Stdout(bytes)
            | Frame::Stderr(bytes) => bytes,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub async fn write<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> io::Result<()> {
    let payload = frame.payload();
    assert!(payload.len() <= MAXIMUM_PAYLOAD, "frames are chunked");
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&(payload.len() as u32 + 1).to_be_bytes());
    header[4] = frame.kind();
    writer.write_all(&header).await?;
    writer.write_all(payload).await
}

/// Reads one frame; `None` at a clean end of stream between frames.
pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => (),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAXIMUM_PAYLOAD + 1 {
        return Err(invalid("frame length out of range"));
    }
    let mut kind = [0u8; 1];
    reader.read_exact(&mut kind).await?;
    let mut payload = vec![0u8; length - 1];
    reader.read_exact(&mut payload).await?;
    Ok(Some(match kind[0] {
        0 if payload.is_empty() => Frame::Heartbeat,
        1 => Frame::Message(payload),
        2 => Frame::Content(payload),
        3 => Frame::Stdin(payload),
        4 => Frame::Stdout(payload),
        5 => Frame::Stderr(payload),
        _ => return Err(invalid("unknown frame")),
    }))
}

/// Byte strings in JSON messages, such as arguments and environment values, which
/// need not be UTF-8.
pub mod bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(value))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD
            .decode(text)
            .map_err(|_| serde::de::Error::custom("invalid base64"))
    }

    pub mod list {
        use super::*;
        use serde::ser::SerializeSeq;

        pub fn serialize<S: Serializer>(
            values: &[Vec<u8>],
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            let mut seq = serializer.serialize_seq(Some(values.len()))?;
            for value in values {
                seq.serialize_element(&STANDARD.encode(value))?;
            }
            seq.end()
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<Vec<u8>>, D::Error> {
            Vec::<String>::deserialize(deserializer)?
                .into_iter()
                .map(|text| {
                    STANDARD
                        .decode(text)
                        .map_err(|_| serde::de::Error::custom("invalid base64"))
                })
                .collect()
        }
    }

    pub mod optional {
        use super::*;
        pub fn serialize<S: Serializer>(
            value: &Option<Vec<u8>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => serializer.serialize_some(&STANDARD.encode(value)),
                None => serializer.serialize_none(),
            }
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Vec<u8>>, D::Error> {
            Option::<String>::deserialize(deserializer)?
                .map(|text| {
                    STANDARD
                        .decode(text)
                        .map_err(|_| serde::de::Error::custom("invalid base64"))
                })
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_reject_garbage() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let frames = [
            Frame::Heartbeat,
            Frame::Message(b"{}".to_vec()),
            Frame::Content(vec![0; 1000]),
            Frame::Content(Vec::new()),
            Frame::Stdout(vec![255, 0, 128]),
        ];
        for frame in &frames {
            write(&mut a, frame).await.unwrap();
        }
        drop(a);
        for frame in &frames {
            assert_eq!(read(&mut b).await.unwrap().as_ref(), Some(frame));
        }
        assert_eq!(read(&mut b).await.unwrap(), None);

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0xff, 0xff, 0xff, 0xff, 1]).await.unwrap();
        assert!(read(&mut b).await.is_err());
    }
}
