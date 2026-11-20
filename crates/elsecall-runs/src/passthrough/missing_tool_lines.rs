//! Recognising, in a placed run's output as it passes through the caller, the
//! line that says a tool is missing: a shell's "command not found", `env` or
//! `make` failing to find a program, the `xcode-select` and `xcrun` stubs that
//! need Xcode, and mise saying a tool is not installed.
//!
//! Output is never kept. Each stream's current line is held up to `MAXIMUM_LINE`
//! bytes, a longer line is skipped whole, and a line is dropped once examined.
//! Only a recognised tool's name survives, validated as `ToolName`, which never
//! takes a credential's shape. The bytes themselves pass through untouched, so
//! what the caller's streams receive is exactly what the command wrote.
use elsecall_scheduling::missing_tool::{Manager, MissingTool, ToolName};

/// The longest line examined. Every message recognised here fits easily.
const MAXIMUM_LINE: usize = 512;

/// One stream's line in progress.
#[derive(Default)]
struct Line {
    bytes: Vec<u8>,
    /// The line outgrew `MAXIMUM_LINE`; skip it up to its end.
    skipping: bool,
}

impl Line {
    fn feed(&mut self, chunk: &[u8], found: &mut Found) {
        let mut rest = chunk;
        while !rest.is_empty() {
            let end = rest.iter().position(|b| *b == b'\n');
            let piece = &rest[..end.unwrap_or(rest.len())];
            if !self.skipping {
                if self.bytes.len() + piece.len() > MAXIMUM_LINE {
                    self.skipping = true;
                    self.bytes.clear();
                } else {
                    self.bytes.extend_from_slice(piece);
                }
            }
            let Some(end) = end else { return };
            self.end(found);
            rest = &rest[end + 1..];
        }
    }

    fn end(&mut self, found: &mut Found) {
        if !self.skipping {
            found.consider(&self.bytes);
        }
        self.bytes.clear();
        self.skipping = false;
    }
}

/// The latest tool recognised, and how to tell a program that exists on the
/// caller from a typo.
struct Found {
    tool: Option<MissingTool>,
    exists_here: fn(&str) -> bool,
}

impl Found {
    fn consider(&mut self, line: &[u8]) {
        let kept = match recognise(line) {
            // Xcode's stubs are macOS's own words, about Xcode itself.
            Some(Recognised::Managed(missing)) if missing.manager == Manager::Xcodes => {
                Some(missing)
            }
            // A program, or a tool mise named, that the caller lacks too is a
            // typo, missing everywhere, or a name the job's own output made up
            // (`npm:anything`): nothing a pin on the master would fix, and
            // nothing to suggest installing.
            Some(Recognised::Program(missing) | Recognised::Managed(missing)) => {
                (self.exists_here)(missing.tool.as_str()).then_some(missing)
            }
            None => None,
        };
        if kept.is_some() {
            self.tool = kept;
        }
    }
}

/// Watches a run's two output streams; the latest recognised tool wins, since
/// the line nearest the failure names what failed it.
pub(crate) struct Recogniser {
    stdout: Line,
    stderr: Line,
    found: Found,
}

impl Recogniser {
    /// `exists_here` says whether a program by that name is on the caller's
    /// `PATH`; a program recognised as missing counts only if it is.
    pub(crate) fn new(exists_here: fn(&str) -> bool) -> Self {
        Self {
            stdout: Line::default(),
            stderr: Line::default(),
            found: Found {
                tool: None,
                exists_here,
            },
        }
    }

    pub(crate) fn stdout(&mut self, chunk: &[u8]) {
        self.stdout.feed(chunk, &mut self.found);
    }

    pub(crate) fn stderr(&mut self, chunk: &[u8]) {
        self.stderr.feed(chunk, &mut self.found);
    }

    /// The tool recognised, after examining unfinished last lines.
    pub(crate) fn finish(mut self) -> Option<MissingTool> {
        self.stdout.end(&mut self.found);
        self.stderr.end(&mut self.found);
        self.found.tool
    }
}

/// What a line said is missing: a program a command line called, or a tool a
/// manager (mise, Xcode) lacks.
#[derive(Debug, PartialEq)]
pub(crate) enum Recognised {
    Program(MissingTool),
    Managed(MissingTool),
}

/// The program `name` as a missing tool, for a command that could not start.
pub(crate) fn missing_program(name: &str) -> Option<MissingTool> {
    program(name)
}

/// `line` without terminal escape sequences (colours) and surrounding space.
fn plain(line: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(line).ok()?;
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ parameters, ended by a letter.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        plain.push(c);
    }
    Some(plain.trim().to_owned())
}

/// The missing tool `line` names, if it is one of the recognised messages.
fn recognise(line: &[u8]) -> Option<Recognised> {
    let line = plain(line)?;
    xcode(&line)
        .or_else(|| mise(&line))
        .map(Recognised::Managed)
        .or_else(|| {
            command(&line)
                .and_then(|name| program(&name))
                .map(Recognised::Program)
        })
}

/// A program a command line called, as a shell, `env` or `make` reports it.
/// Paths (`./build.sh`) are files, not tools.
fn program(name: &str) -> Option<MissingTool> {
    let name = name.trim_matches(['\'', '"', '`']);
    if name.contains('/') {
        return None;
    }
    Some(MissingTool {
        manager: Manager::Mise,
        tool: ToolName::new(name)?,
        called: None,
    })
}

/// The program in a "not found" message:
/// - zsh: `zsh: command not found: X`
/// - bash and sh: `bash: X: command not found`, `script.sh: line 3: X: command not found`
/// - make: `make: X: Command not found`, `make: X: No such file or directory`
/// - env, from a script's `#!/usr/bin/env X`: `env: X: No such file or directory`
fn command(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if let Some(at) = lower.find("command not found: ") {
        return Some(line[at + "command not found: ".len()..].trim().to_owned());
    }
    for ending in [": command not found", ": no such file or directory"] {
        let Some(head) = lower
            .ends_with(ending)
            .then(|| &line[..line.len() - ending.len()])
        else {
            continue;
        };
        if ending == ": no such file or directory"
            && !["env: ", "/usr/bin/env: ", "make: "]
                .iter()
                .any(|prefix| head.starts_with(prefix))
        {
            return None;
        }
        return head.rsplit(": ").next().map(str::to_owned);
    }
    None
}

/// The stubs macOS puts in place of Xcode's tools when only the Command Line
/// Tools are installed:
/// - `xcode-select: error: tool 'xcodebuild' requires Xcode, but active developer directory …`
/// - `xcrun: error: unable to find utility "simctl", not a developer tool or in PATH`
fn xcode(line: &str) -> Option<MissingTool> {
    let quoted = |rest: &str, quote: char| -> Option<String> {
        let rest = rest.strip_prefix(quote)?;
        Some(rest[..rest.find(quote)?].to_owned())
    };
    let called = if let Some(rest) = line.strip_prefix("xcode-select: error: tool ") {
        rest.contains("requires Xcode")
            .then(|| quoted(rest, '\''))
            .flatten()?
    } else {
        let rest = line.strip_prefix("xcrun: error: unable to find utility ")?;
        quoted(rest, '"')?
    };
    Some(MissingTool {
        manager: Manager::Xcodes,
        tool: ToolName::new("xcode")?,
        called: Some(ToolName::new(&called).filter(|name| !name.as_str().contains('/'))?),
    })
}

/// mise reporting a tool it has no version of:
/// - `mise ERROR No version is set for shim: X`
/// - `mise ERROR X@1.2.3 is not installed`, `mise WARN missing: X@1.2.3 Y@2`
fn mise(line: &str) -> Option<MissingTool> {
    let rest = line.strip_prefix("mise ")?;
    let name = if let Some(at) = rest.find("No version is set for shim: ") {
        rest[at + "No version is set for shim: ".len()..]
            .split_whitespace()
            .next()?
            .to_owned()
    } else if rest.contains("not installed") || rest.contains("missing:") {
        let versioned = rest
            .split_whitespace()
            .map(|word| word.trim_matches(|c: char| ",;:'\"`()".contains(c)))
            .find(|word| word.len() > 1 && word[1..].contains('@'))?;
        versioned[..versioned.rfind('@')?].to_owned()
    } else {
        return None;
    };
    Some(MissingTool {
        manager: Manager::Mise,
        tool: ToolName::new(&name)?,
        called: None,
    })
}

#[cfg(test)]
mod tests {
    use crate::passthrough::missing_tool_lines::*;

    fn tool(line: &str) -> Option<(Manager, String, Option<String>)> {
        recognise(line.as_bytes()).map(|recognised| {
            let m = match recognised {
                Recognised::Program(m) | Recognised::Managed(m) => m,
            };
            (
                m.manager,
                m.tool.to_string(),
                m.called.map(|c| c.to_string()),
            )
        })
    }

    fn mise_tool(name: &str) -> Option<(Manager, String, Option<String>)> {
        Some((Manager::Mise, name.into(), None))
    }

    #[test]
    fn shells_env_and_make_name_the_program() {
        assert_eq!(tool("zsh: command not found: tuist"), mise_tool("tuist"));
        assert_eq!(
            tool("bash: swiftgen: command not found"),
            mise_tool("swiftgen")
        );
        assert_eq!(
            tool("/w/app/scripts/ci.sh: line 3: bun: command not found"),
            mise_tool("bun")
        );
        assert_eq!(
            tool("sh: line 1: node: command not found"),
            mise_tool("node")
        );
        assert_eq!(
            tool("make: xcbeautify: Command not found"),
            mise_tool("xcbeautify")
        );
        assert_eq!(
            tool("make: sentry-cli: No such file or directory"),
            mise_tool("sentry-cli")
        );
        assert_eq!(
            tool("env: node: No such file or directory"),
            mise_tool("node")
        );
        assert_eq!(
            tool("/usr/bin/env: 'python3': No such file or directory"),
            mise_tool("python3")
        );
        // Files and ordinary errors are not tools.
        assert_eq!(tool("bash: ./build.sh: No such file or directory"), None);
        assert_eq!(tool("cat: notes.txt: No such file or directory"), None);
        assert_eq!(tool("zsh: command not found: ./x"), None);
        assert_eq!(tool("error: tests failed"), None);
    }

    #[test]
    fn xcode_stubs_name_xcode_and_the_utility() {
        assert_eq!(
            tool(
                "xcode-select: error: tool 'xcodebuild' requires Xcode, but active developer directory '/Library/Developer/CommandLineTools' is a command line tools instance"
            ),
            Some((Manager::Xcodes, "xcode".into(), Some("xcodebuild".into())))
        );
        assert_eq!(
            tool(
                "xcrun: error: unable to find utility \"simctl\", not a developer tool or in PATH"
            ),
            Some((Manager::Xcodes, "xcode".into(), Some("simctl".into())))
        );
    }

    #[test]
    fn mise_names_the_tool_without_its_version() {
        assert_eq!(
            tool("mise ERROR No version is set for shim: bun"),
            mise_tool("bun")
        );
        assert_eq!(
            tool("\u{1b}[31mmise\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m node@22.19.0 is not installed"),
            mise_tool("node")
        );
        assert_eq!(
            tool("mise WARN  missing: npm:@scope/pkg@1.2.0 bun@1.3"),
            mise_tool("npm:@scope/pkg")
        );
        assert_eq!(tool("mise WARN  some other warning"), None);
    }

    #[test]
    fn credential_shaped_names_are_never_kept() {
        // Assembled at run time, as a script running `$TOKEN` by mistake prints it.
        let token = format!("gh{}", "p_aB3dE5fG7hJ9".repeat(3));
        assert_eq!(tool(&format!("bash: {token}: command not found")), None);
        assert_eq!(tool(&format!("zsh: command not found: {token}")), None);
    }

    #[test]
    fn lines_split_across_chunks_are_seen_and_long_lines_are_skipped() {
        let mut recogniser = Recogniser::new(|_| true);
        recogniser.stderr(b"building\nzsh: command not fo");
        recogniser.stdout(&vec![b'x'; 100_000]);
        recogniser.stderr(b"und: tuist\n");
        assert_eq!(
            recogniser.finish().map(|m| m.tool.to_string()),
            Some("tuist".into())
        );

        // A line longer than the bound is skipped whole, even if it ends in a match.
        let mut recogniser = Recogniser::new(|_| true);
        recogniser.stderr(&vec![b'y'; MAXIMUM_LINE]);
        recogniser.stderr(b": command not found\n");
        assert_eq!(recogniser.finish(), None);

        // The last recognised line wins; an unfinished last line still counts.
        let mut recogniser = Recogniser::new(|_| true);
        recogniser
            .stderr(b"bash: bun: command not found\nmise ERROR No version is set for shim: node");
        assert_eq!(
            recogniser.finish().map(|m| m.tool.to_string()),
            Some("node".into())
        );

        // A program the caller lacks as well is a typo, not a missing pin.
        let mut recogniser = Recogniser::new(|name| name == "bun");
        recogniser.stderr(b"bash: bun: command not found\nbash: mkae: command not found\n");
        assert_eq!(
            recogniser.finish().map(|m| m.tool.to_string()),
            Some("bun".into())
        );
        // So is a mise tool the caller has no program for, such as a backend
        // name the job printed.
        let mut recogniser = Recogniser::new(|name| name == "bun");
        recogniser.stderr(
            b"mise WARN  missing: npm:lure@1 bun@1.3\nmise ERROR npm:lure@1 is not installed\n",
        );
        assert_eq!(recogniser.finish(), None);
        let mut recogniser = Recogniser::new(|_| false);
        recogniser.stderr(
            b"xcrun: error: unable to find utility \"simctl\", not a developer tool or in PATH\n",
        );
        assert_eq!(
            recogniser.finish().map(|m| m.tool.to_string()),
            Some("xcode".into())
        );
        assert_eq!(missing_program("./x"), None);
        assert_eq!(
            missing_program("tuist").map(|m| m.tool.to_string()),
            Some("tuist".into())
        );
    }
}
