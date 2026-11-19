//! The guide and its topics, as `skill/elsecall/` held them when this binary was
//! built: `SKILL.md`, with the skill's name and description in its frontmatter, and
//! one `references/NAME.md` per topic.

const SKILL: &str = include_str!("../../../skill/elsecall/SKILL.md");

pub struct Topic {
    pub name: &'static str,
    pub text: &'static str,
}

pub const TOPICS: [Topic; 1] = [Topic {
    name: "integrate",
    text: include_str!("../../../skill/elsecall/references/integrate.md"),
}];

/// `SKILL.md`'s frontmatter, and the guide after it.
fn split() -> (&'static str, &'static str) {
    let rest = SKILL
        .strip_prefix("---\n")
        .expect("SKILL.md opens with frontmatter");
    let (frontmatter, guide) = rest.split_once("\n---\n").expect("frontmatter closes");
    (frontmatter, guide.trim_start_matches('\n'))
}

/// The guide `elsecall skill show` prints: `SKILL.md` without its frontmatter.
pub fn guide() -> &'static str {
    split().1
}

/// The skill's description, which the stub repeats verbatim.
pub fn description() -> &'static str {
    split()
        .0
        .lines()
        .find_map(|line| line.strip_prefix("description: "))
        .expect("SKILL.md has a description")
}

pub fn topic(name: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|topic| topic.name == name)
}

pub fn names() -> Vec<&'static str> {
    TOPICS.iter().map(|topic| topic.name).collect()
}

/// The whole skill folder, as relative paths and contents.
pub fn files() -> Vec<(String, &'static str)> {
    let mut files = vec![("SKILL.md".to_owned(), SKILL)];
    files.extend(
        TOPICS
            .iter()
            .map(|topic| (format!("references/{}.md", topic.name), topic.text)),
    );
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names the guide's "Topics" list offers, as in "- `integrate`: ...".
    fn listed() -> Vec<&'static str> {
        let topics = guide()
            .split("\n## Topics\n")
            .nth(1)
            .expect("a Topics section");
        topics
            .lines()
            .take_while(|line| !line.starts_with("## "))
            .filter_map(|line| line.strip_prefix("- `"))
            .filter_map(|rest| rest.split_once('`').map(|(name, _)| name))
            .collect()
    }

    #[test]
    fn the_guide_offers_exactly_the_embedded_topics() {
        assert_eq!(listed(), names());
        for topic in &TOPICS {
            assert!(topic.text.starts_with("# "), "{} has a title", topic.name);
        }
        assert!(topic("nothing").is_none());
    }

    #[test]
    fn the_guide_prints_without_its_frontmatter_and_links_no_reference_file() {
        assert!(guide().starts_with("# Elsecall\n"));
        assert!(SKILL.starts_with("---\nname: elsecall\ndescription: "));
        for text in std::iter::once(guide()).chain(TOPICS.iter().map(|t| t.text)) {
            assert!(!text.contains("](references/"), "a reference link");
            assert!(!text.contains("](./"), "a relative link");
        }
    }
}
