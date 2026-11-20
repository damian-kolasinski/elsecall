//! `.tool-versions`, the asdf format mise also reads: one tool per line, its
//! name and then one or more versions, `#` starting a comment.
use crate::{
    pins::{invalid, literal_version, mise_config::ToolRequests},
    tool_inventory::ToolName,
};
use elsecall_contract::failure::Result;

const MAX_LINES: usize = 1024;

/// The tools `text` pins, in file order. `file` names the file in errors.
pub(super) fn tools(text: &str, file: &str) -> Result<Vec<ToolRequests>> {
    let mut requests: Vec<ToolRequests> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if index >= MAX_LINES {
            return Err(invalid(file, "", "at most 1024 lines"));
        }
        let key = format!("line {}", index + 1);
        let content = line.split('#').next().unwrap_or_default();
        let mut words = content.split_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        let tool = ToolName::new(name)
            .ok_or_else(|| invalid(file, &key, "tool names cannot contain control characters"))?;
        let versions = words
            .map(|word| literal_version(file, &key, word))
            .collect::<Result<Vec<_>>>()?;
        if versions.is_empty() {
            return Err(invalid(file, &key, "a tool needs at least one version"));
        }
        if requests.iter().any(|request| request.tool == tool) {
            return Err(invalid(file, &key, "a tool is listed on two lines"));
        }
        requests.push(ToolRequests { tool, versions });
    }
    Ok(requests)
}

#[cfg(test)]
mod tests {
    use crate::pins::tool_versions::*;

    #[test]
    fn lines_name_a_tool_and_its_versions() {
        let text = "# pinned for CI\nnode 22.19.0 20.19.5\n\npython 3.10.4 # the app's scripts\n";
        let requests = tools(text, ".tool-versions").unwrap();
        let found: Vec<(&str, Vec<&str>)> = requests
            .iter()
            .map(|request| {
                (
                    request.tool.as_str(),
                    request
                        .versions
                        .iter()
                        .map(|version| version.as_str())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("node", vec!["22.19.0", "20.19.5"]),
                ("python", vec!["3.10.4"]),
            ]
        );
    }

    #[test]
    fn malformed_lines_are_named_without_their_values() {
        for (text, message) in [
            (
                "node\n",
                ".tool-versions: line 1: a tool needs at least one version.",
            ),
            (
                "node 1\nnode 2\n",
                ".tool-versions: line 2: a tool is listed on two lines.",
            ),
            (
                "\nnode {{SECRET}}\n",
                ".tool-versions: line 2: versions must be literal; Elsecall does not render templates.",
            ),
        ] {
            let error = tools(text, ".tool-versions").unwrap_err();
            assert_eq!(error.code, "pin_source_invalid");
            assert_eq!(error.message, message);
        }
    }
}
