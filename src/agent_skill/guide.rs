//! The guide and its topics, as `skill/` holds them when this binary was built.

pub const GUIDE: &str = include_str!("../../skill/guide.md");

pub struct Topic {
    pub name: &'static str,
    pub text: &'static str,
}

pub const TOPICS: [Topic; 1] = [Topic {
    name: "integrate",
    text: include_str!("../../skill/integrate.md"),
}];

pub fn topic(name: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|topic| topic.name == name)
}

pub fn names() -> Vec<&'static str> {
    TOPICS.iter().map(|topic| topic.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names the guide's "Topics" list offers, as in "- `integrate`: ...".
    fn listed() -> Vec<&'static str> {
        let topics = GUIDE
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
}
