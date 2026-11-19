//! Reads one file of a local channel with curl, over HTTP(S) or from a directory.
use elsecall_failure::{Failure, Result};
use std::{path::Path, time::Duration};

/// Reads `url` into `output` when given, else into memory, refusing more than
/// `max_bytes`.
pub fn fetch(url: &str, max_bytes: u64, output: Option<&Path>) -> Result<Vec<u8>> {
    let limit = max_bytes.to_string();
    let mut arguments = vec![
        "-q",
        "--fail",
        "--silent",
        "--location",
        "--proto",
        "=http,https,file",
        // A server may redirect within the web, never into local files.
        "--proto-redir",
        "=http,https",
        "--connect-timeout",
        "10",
        "--max-time",
        "120",
        "--max-filesize",
        &limit,
    ];
    let output = output.map(|path| path.to_str().expect("UTF8 stage path"));
    if let Some(output) = output {
        arguments.extend(["--output", output]);
    }
    arguments.push(url);
    super::super::tools::run(
        Path::new("/usr/bin/curl"),
        &arguments,
        Path::new("/"),
        Duration::from_secs(130),
        &[],
    )
    .map_err(|_| {
        Failure::new(
            "local_release_unreachable",
            format!("Could not read {url} from the local release channel."),
            "Serve the channel with scripts/local-release.sh serve, or rebuild for the channel's actual URL.",
        )
    })
}
