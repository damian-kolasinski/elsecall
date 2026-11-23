//! Recognize fixed diagnostic codes without storing or returning command output.

use tokio::io::{AsyncRead, AsyncReadExt};

pub(super) type Classify = fn(&[u8]) -> Option<&'static str>;
const LINE_LIMIT: usize = 4096;

pub(super) async fn scan(
    reader: Option<impl AsyncRead + Unpin>,
    classify: Option<Classify>,
    recognized: &mut Option<&'static str>,
) {
    let (Some(mut reader), Some(classify)) = (reader, classify) else {
        return;
    };
    let mut input = [0; LINE_LIMIT];
    let mut line = Vec::with_capacity(LINE_LIMIT);
    let mut oversized = false;
    while let Ok(size) = reader.read(&mut input).await {
        if size == 0 {
            if !oversized && recognized.is_none() {
                *recognized = classify(&line);
            }
            break;
        }
        for chunk in input[..size].split_inclusive(|byte| *byte == b'\n') {
            let finished = chunk.ends_with(b"\n");
            if !oversized && line.len() + chunk.len() <= LINE_LIMIT {
                line.extend_from_slice(chunk);
            } else {
                oversized = true;
            }
            if finished {
                if !oversized && recognized.is_none() {
                    *recognized = classify(&line);
                }
                line.clear();
                oversized = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn skips_oversized_lines_and_drains_through_the_final_line() {
        fn classify(line: &[u8]) -> Option<&'static str> {
            line.starts_with(b"diagnostic").then_some("recognized")
        }
        let mut output = Vec::from(b"diagnostic".as_slice());
        output.resize(LINE_LIMIT + 1, b'x');
        let mut recognized = None;
        scan(Some(output.as_slice()), Some(classify), &mut recognized).await;
        assert_eq!(recognized, None);
        output.extend_from_slice(b"\ndiagnostic");
        scan(Some(output.as_slice()), Some(classify), &mut recognized).await;
        assert_eq!(recognized, Some("recognized"));
    }
}
