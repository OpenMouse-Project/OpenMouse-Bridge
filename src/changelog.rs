//! The release notes, from `CHANGELOG.md` built into the binary: the same
//! file the release workflow publishes as the GitHub release notes and
//! `latest.json`'s `notes`, so what the panel shows always matches the
//! installed version and needs no network.

/// `CHANGELOG.md` as of this build.
pub const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// One `## <version>` section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    pub version: &'a str,
    /// The section's Markdown, trimmed.
    pub body: &'a str,
}

/// Every non-empty entry in `changelog`, in file order (newest first). Read
/// the way `.github/scripts/make-release-files.mjs` reads it: a heading is
/// `## <version>`, optionally in brackets or followed by a date
/// (`## 1.0.0 - 2026-11-20`), and runs to the next `## ` heading.
pub fn entries(changelog: &str) -> Vec<Entry<'_>> {
    let mut entries = Vec::new();
    let mut current: Option<(&str, usize)> = None;
    let mut offset = 0;
    for line in changelog.split_inclusive('\n') {
        if let Some(heading) = line.strip_prefix("## ") {
            if let Some((version, start)) = current.take() {
                push(&mut entries, version, &changelog[start..offset]);
            }
            let version = heading
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_start_matches('[')
                .trim_end_matches(']');
            current = Some((version, offset + line.len()));
        }
        offset += line.len();
    }
    if let Some((version, start)) = current {
        push(&mut entries, version, &changelog[start..]);
    }
    entries
}

fn push<'a>(entries: &mut Vec<Entry<'a>>, version: &'a str, body: &'a str) {
    let body = body.trim();
    if !version.is_empty() && !body.is_empty() {
        entries.push(Entry { version, body });
    }
}

/// The notes for `version` in the built-in changelog.
pub fn notes_for(version: &str) -> Option<&'static str> {
    entries(CHANGELOG)
        .into_iter()
        .find(|entry| entry.version == version)
        .map(|entry| entry.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_build_has_notes_for_its_own_version() {
        // The release preflight refuses a tag without them; this catches it
        // before a tag is ever pushed.
        assert!(notes_for(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn entries_run_to_the_next_heading_and_skip_the_preamble() {
        let changelog = "# Changelog\n\nIntro text.\n\n## 1.1.0 - 2026-12-01\n\n- New\n  wrapped\n\n## [1.0.1]\n- Fix\n\n## 1.0.0\n\n## 0.9.0\n- Old\n";
        assert_eq!(
            entries(changelog),
            [
                Entry {
                    version: "1.1.0",
                    body: "- New\n  wrapped"
                },
                Entry {
                    version: "1.0.1",
                    body: "- Fix"
                },
                // 1.0.0 is empty, so it is left out.
                Entry {
                    version: "0.9.0",
                    body: "- Old"
                },
            ]
        );
    }

    #[test]
    fn windows_line_endings_and_no_entries_are_handled() {
        assert_eq!(
            entries("## 2.0.0\r\n- A\r\n"),
            [Entry {
                version: "2.0.0",
                body: "- A"
            }]
        );
        assert!(entries("# Changelog\n").is_empty());
    }
}
