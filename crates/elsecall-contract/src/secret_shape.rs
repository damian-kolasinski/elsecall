//! Whether a piece of text has the shape of a credential: a well-known token
//! prefix, or a long run that mixes letters and digits as generated keys do.
//!
//! It recognises shapes, never values: callers use it to refuse keeping a word
//! (a tool name read from a command's output) or to flag a file as one to ask
//! about before syncing it. Both mistakes are cheap in that direction: a tool
//! name refused as token-like is simply not named, and a file flagged by mistake
//! costs one question.

/// Prefixes that well-known services put on their tokens, matched exactly.
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "xapp-",
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "AKIA",
    "ASIA",
    "AIza",
    "npm_",
    "pypi-",
    "hf_",
    "dop_v1_",
    "shpat_",
    "eyJ",
];

/// A token-prefixed word is at least this long; shorter ones are ordinary words
/// such as `sk-cli`.
const PREFIXED_MINIMUM: usize = 16;
/// A segment at least this long that mixes letters and digits reads as generated.
const GENERATED_SEGMENT: usize = 12;

/// Whether `word` (one whitespace-free word) looks like a credential: it has a
/// token prefix, or looks generated.
pub fn looks_like_token(word: &str) -> bool {
    token_prefixed(word) || looks_generated(word)
}

/// Whether `word` starts with a well-known service's token prefix.
pub fn token_prefixed(word: &str) -> bool {
    word.len() >= PREFIXED_MINIMUM && TOKEN_PREFIXES.iter().any(|prefix| word.starts_with(prefix))
}

/// Whether `word` holds a long segment that mixes letters and digits, as
/// random keys, hex digests and base64 do, and names such as `darwin23` or
/// `arm64` are too short to.
pub fn looks_generated(word: &str) -> bool {
    word.split(['-', '_', '.', '/', ':', '@']).any(|segment| {
        segment.len() >= GENERATED_SEGMENT
            && segment.bytes().any(|b| b.is_ascii_digit())
            && segment.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

/// Whether a variable or key name says its value is a credential, such as
/// `API_TOKEN` or `db_password`.
pub fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "private_key",
        "access_key",
        "credential",
    ]
    .iter()
    .any(|word| name.contains(word))
}

#[cfg(test)]
mod tests {
    use crate::secret_shape::*;

    /// Credential-shaped samples are assembled at run time, so this file never
    /// holds one a scanner would flag.
    fn sample(prefix: &str, body: &str, repeat: usize) -> String {
        format!("{prefix}{}", body.repeat(repeat))
    }

    #[test]
    fn tool_names_are_not_tokens() {
        for name in [
            "node",
            "sentry-cli",
            "swift-format-510.1.0",
            "google-chrome-stable",
            "x86_64-apple-darwin23-gcc",
            "tuist-4.205.0-arm64",
            "python3.10-config",
            "npm:@scope/pkg",
            "sk-cli",
            "kotlin-language-server",
        ] {
            assert!(!looks_like_token(name), "{name}");
        }
    }

    #[test]
    fn prefixed_and_generated_words_are_tokens() {
        for token in [
            sample("gh", "p_aB3dE5fG7hJ9", 3),
            sample("AK", "IA7Q2W", 3),
            sample("xo", "xb-1234-abcd", 2),
            sample("", "0f9e8d7c6b5a", 2),
            sample("", "Hb7EwlL7vlg8", 2),
            sample("ey", "JhbGciOiJIUzI1", 2),
        ] {
            assert!(looks_like_token(&token), "{token}");
        }
    }

    #[test]
    fn credential_names_are_recognised() {
        for name in [
            "API_TOKEN",
            "db_password",
            "STRIPE_SECRET",
            "AWS_ACCESS_KEY_ID",
        ] {
            assert!(secret_name(name), "{name}");
        }
        for name in ["PATH", "LOG_LEVEL", "SCHEME", "AUTHOR"] {
            assert!(!secret_name(name), "{name}");
        }
    }
}
