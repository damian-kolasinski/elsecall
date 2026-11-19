//! Glob patterns over labels, as label rules and reports select labels with.

/// Matches `text` against a glob `pattern`: `*` matches any run of characters,
/// `?` any one character, and every other character itself.
pub fn matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // The last `*` seen, and the text position it currently extends to.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(c) if *c == '?' || *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    star = Some((star_p, star_t + 1));
                    p = star_p + 1;
                    t = star_t + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns() {
        assert!(matches("test *", "test FeatureModule"));
        assert!(matches("*Module", "test FeatureModule"));
        assert!(matches("a*b*c", "aXbYc"));
        assert!(matches("a*b*c", "abbc"));
        assert!(!matches("a*b*c", "aXcYb"));
        assert!(!matches("test", "test x"));
        assert!(matches("*", ""));
        assert!(matches("build ?", "build A"));
        assert!(!matches("build ?", "build AB"));
        assert!(matches("ü*", "über"));
        assert!(matches("*a", "banana"));
        assert!(!matches("*a", "bananas"));
    }
}
