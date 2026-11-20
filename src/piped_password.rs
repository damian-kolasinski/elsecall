//! A password a program passes on standard input, for the commands that need
//! one: `fleet add` and `service start-at-boot`.
use elsecall_contract::failure::{Failure, Result};
use std::io::{BufRead, Read};
use zeroize::Zeroizing;

const PASSWORD_LIMIT: u64 = 4096;

/// The password: the first line of `input`, read no further, so a
/// program can keep the rest open. A terminal is refused, so the command never
/// waits for typing or echoes the password.
pub(crate) fn read(input: &mut impl BufRead, terminal: bool) -> Result<Zeroizing<String>> {
    let refused = || {
        Failure::new(
            "password_required",
            "The password must arrive on standard input.",
            "Pipe it in, for example from your password manager's command-line tool.",
        )
    };
    if terminal {
        return Err(refused());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    input
        .take(PASSWORD_LIMIT)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| Failure::io("Read the password", error))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| refused())?;
    let line = text.split('\n').next().unwrap_or_default();
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        return Err(refused());
    }
    Ok(Zeroizing::new(line.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_is_the_first_line_of_piped_input() {
        let parse = |input: &[u8], terminal| read(&mut { input }, terminal).map(|p| p.to_string());
        assert_eq!(parse(b"s3cret pass\n", false).unwrap(), "s3cret pass");
        assert_eq!(parse(b"s3cret\r\nignored\n", false).unwrap(), "s3cret");
        assert_eq!(parse(b"no newline", false).unwrap(), "no newline");
        for refused in [&b""[..], b"\n", b"\xff\xfe\n"] {
            assert_eq!(parse(refused, false).unwrap_err().code, "password_required");
        }
        assert_eq!(
            parse(b"typed\n", true).unwrap_err().code,
            "password_required"
        );
    }
}
