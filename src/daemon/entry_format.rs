use std::collections::HashSet;
use std::path::PathBuf;

use crate::{
    error::{Error, Result},
    paths,
    protocol::{ControlRequest, EntryState},
    state::AccessMode,
};

/// Render a slice of entries into a column-aligned table.
///
/// Columns are in `ENTRY TARGET MODE` order. When `comment_header` is `true`
/// the two header lines are prefixed with `# ` (for the edit buffer); when
/// `false` they are printed bare (for `status`).  Every line ends with `\n`.
pub(crate) fn render_entries(entries: &[EntryState], comment_header: bool) -> String {
    const ENTRY_LABEL: &str = "ENTRY";
    const TARGET_LABEL: &str = "TARGET";
    const MODE_LABEL: &str = "MODE";
    const ENTRY_RULE: &str = "-----";
    const TARGET_RULE: &str = "------";

    // The comment marker is part of the first column's content (not an external
    // prefix), so the header/rule rows stay aligned with the unprefixed data rows.
    let prefix = if comment_header { "# " } else { "" };
    let entry_header = format!("{prefix}{ENTRY_LABEL}");
    let entry_rule = format!("{prefix}{ENTRY_RULE}");

    // Compute column widths.
    let entry_width = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0)
        .max(entry_header.len());
    let target_width = entries
        .iter()
        .map(|e| e.target.display().to_string().len())
        .max()
        .unwrap_or(0)
        .max(TARGET_LABEL.len());

    let mut buf = String::new();

    // Header lines.
    buf.push_str(&format!(
        "{:<entry_width$} {:<target_width$} {}\n",
        entry_header,
        TARGET_LABEL,
        MODE_LABEL,
        entry_width = entry_width,
        target_width = target_width,
    ));
    buf.push_str(&format!(
        "{:<entry_width$} {:<target_width$} {}\n",
        entry_rule,
        TARGET_RULE,
        "----",
        entry_width = entry_width,
        target_width = target_width,
    ));

    // Data rows.
    for entry in entries {
        let mode = mode_str(entry.mode);
        let target = entry.target.display().to_string();
        buf.push_str(&format!(
            "{:<entry_width$} {:<target_width$} {}\n",
            entry.name,
            target,
            mode,
            entry_width = entry_width,
            target_width = target_width,
        ));
    }

    buf
}

/// Parse an entry table (as produced by `render_entries`) back into
/// `Vec<EntryState>`.
///
/// Blank lines and lines whose first non-whitespace character is `#` are
/// ignored. For each content line the first whitespace-separated token is
/// `name`, the last is `mode`, and the trimmed span between them is `target`.
pub(crate) fn parse_entries(text: &str) -> Result<Vec<EntryState>> {
    let mut entries = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();

        // Skip blank lines and comment lines.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on whitespace to find the first and last tokens.
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 3 {
            return Err(Error::Cli(format!(
                "entry line must have at least 3 tokens (name, target, mode), got: {raw_line:?}"
            )));
        }

        let name = tokens[0].to_owned();
        let mode_str = tokens[tokens.len() - 1];

        // The target is the trimmed span between the first and last token.
        // Find the byte index where the name token ends and where the mode
        // token begins in the raw line, then slice and trim.
        let name_end_in_raw = {
            let start = raw_line.find(tokens[0]).unwrap_or(0);
            start + tokens[0].len()
        };
        let mode_start_in_raw = raw_line.rfind(mode_str).unwrap_or(raw_line.len());

        let target_raw = &raw_line[name_end_in_raw..mode_start_in_raw];
        let target_str = target_raw.trim();

        if target_str.is_empty() {
            return Err(Error::Cli(format!("target is empty on line: {raw_line:?}")));
        }

        let mode = parse_mode(mode_str, raw_line)?;
        let target = PathBuf::from(target_str);

        entries.push(EntryState { name, target, mode });
    }

    Ok(entries)
}

/// Validate a slice of entries:
/// - each name passes `paths::validate_entry_name`
/// - no entry has an empty/whitespace-only target
/// - no duplicate names
pub(crate) fn validate_entries(entries: &[EntryState]) -> Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();

    for entry in entries {
        paths::validate_entry_name(&entry.name)?;

        let target_str = entry.target.display().to_string();
        if target_str.trim().is_empty() {
            return Err(Error::Cli(format!(
                "entry {:?} has an empty target",
                entry.name
            )));
        }

        if !seen.insert(entry.name.as_str()) {
            return Err(Error::Cli(format!(
                "duplicate entry name: {:?}",
                entry.name
            )));
        }
    }

    Ok(())
}

/// Compute the minimal diff between `before` and `after`, returning the
/// `ControlRequest`s needed to bring the daemon from `before` to `after`.
///
/// - Added entries or entries with changed target/mode → `Add { replace: true }`
/// - Entries removed from `after` → `Remove`
/// - Unchanged entries → no request
///
/// Adds are emitted in `after` order; Removes in `before` order.
pub(crate) fn plan_edit(before: &[EntryState], after: &[EntryState]) -> Vec<ControlRequest> {
    let before_map: std::collections::HashMap<&str, &EntryState> =
        before.iter().map(|e| (e.name.as_str(), e)).collect();
    let after_names: HashSet<&str> = after.iter().map(|e| e.name.as_str()).collect();

    let mut requests = Vec::new();

    // Adds and modifications (in `after` order).
    for entry in after {
        match before_map.get(entry.name.as_str()) {
            None => {
                // New entry.
                requests.push(ControlRequest::Add {
                    name: entry.name.clone(),
                    target: entry.target.clone(),
                    mode: entry.mode,
                    replace: true,
                });
            }
            Some(prev) => {
                if prev.target != entry.target || prev.mode != entry.mode {
                    // Changed entry.
                    requests.push(ControlRequest::Add {
                        name: entry.name.clone(),
                        target: entry.target.clone(),
                        mode: entry.mode,
                        replace: true,
                    });
                }
                // Unchanged → no request.
            }
        }
    }

    // Removes (in `before` order).
    for entry in before {
        if !after_names.contains(entry.name.as_str()) {
            requests.push(ControlRequest::Remove {
                name: entry.name.clone(),
            });
        }
    }

    requests
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mode_str(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::ReadOnly => "ro",
        AccessMode::ReadWrite => "rw",
    }
}

fn parse_mode(token: &str, raw_line: &str) -> Result<AccessMode> {
    match token {
        "ro" => Ok(AccessMode::ReadOnly),
        "rw" => Ok(AccessMode::ReadWrite),
        other => Err(Error::Cli(format!(
            "invalid mode token {other:?} on line: {raw_line:?} (expected `ro` or `rw`)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, target: &str, mode: AccessMode) -> EntryState {
        EntryState {
            name: name.to_owned(),
            target: PathBuf::from(target),
            mode,
        }
    }

    // -- plan_edit -----------------------------------------------------------

    #[test]
    fn plan_edit_covers_all_change_kinds() {
        let before = vec![
            entry("added_later", "/tmp/irrelevant", AccessMode::ReadOnly), // not in before — we test the reverse
            entry("mode_change", "/tmp/mc", AccessMode::ReadOnly),
            entry("target_change", "/tmp/old", AccessMode::ReadWrite),
            entry("unchanged", "/tmp/same", AccessMode::ReadOnly),
            entry("dropped", "/tmp/drop", AccessMode::ReadWrite),
        ];

        let after = vec![
            entry("new_entry", "/tmp/new", AccessMode::ReadWrite), // added
            entry("mode_change", "/tmp/mc", AccessMode::ReadWrite), // mode flipped
            entry("target_change", "/tmp/new_target", AccessMode::ReadWrite), // target changed
            entry("unchanged", "/tmp/same", AccessMode::ReadOnly), // identical
                                                                   // "dropped" is absent
        ];

        let requests = plan_edit(&before, &after);

        // Collect adds and removes.
        let adds: Vec<_> = requests
            .iter()
            .filter_map(|r| {
                if let ControlRequest::Add {
                    name,
                    target,
                    mode,
                    replace,
                } = r
                {
                    Some((name.as_str(), target.clone(), *mode, *replace))
                } else {
                    None
                }
            })
            .collect();

        let removes: Vec<_> = requests
            .iter()
            .filter_map(|r| {
                if let ControlRequest::Remove { name } = r {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect();

        // new_entry added.
        assert!(adds.iter().any(|(n, _, _, r)| *n == "new_entry" && *r));
        // mode_change updated.
        assert!(
            adds.iter()
                .any(|(n, _, m, r)| *n == "mode_change" && *m == AccessMode::ReadWrite && *r)
        );
        // target_change updated.
        assert!(adds.iter().any(|(n, t, _, r)| *n == "target_change"
            && t == &PathBuf::from("/tmp/new_target")
            && *r));
        // unchanged produces no Add.
        assert!(!adds.iter().any(|(n, _, _, _)| *n == "unchanged"));
        // dropped produces a Remove.
        assert!(removes.contains(&"dropped"));
        // no Remove for anything still present.
        assert!(!removes.contains(&"unchanged"));
        assert!(!removes.contains(&"new_entry"));
    }

    #[test]
    fn plan_edit_unchanged_is_empty() {
        let entries = vec![
            entry("docs", "/home/user/docs", AccessMode::ReadOnly),
            entry("build", "/tmp/build", AccessMode::ReadWrite),
        ];
        assert!(plan_edit(&entries, &entries).is_empty());
    }

    // -- round-trip ----------------------------------------------------------

    #[test]
    fn render_parse_round_trip_is_identity() {
        let entries = vec![
            entry("docs", "/home/user/project/docs", AccessMode::ReadOnly),
            entry("build", "/tmp/build-out", AccessMode::ReadWrite),
        ];

        let rendered = render_entries(&entries, true);
        let parsed = parse_entries(&rendered).expect("parse should succeed");
        assert_eq!(parsed, entries);

        let diff = plan_edit(&entries, &parsed);
        assert!(
            diff.is_empty(),
            "round-tripped entries must produce empty diff"
        );
    }

    // -- validate_entries ----------------------------------------------------

    #[test]
    fn validate_rejects_duplicate_name() {
        let entries = vec![
            entry("same", "/tmp/a", AccessMode::ReadOnly),
            entry("same", "/tmp/b", AccessMode::ReadWrite),
        ];
        let err = validate_entries(&entries).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "err: {err}");
    }

    #[test]
    fn validate_rejects_invalid_entry_name() {
        let entries = vec![entry("..", "/tmp/target", AccessMode::ReadOnly)];
        assert!(validate_entries(&entries).is_err());
    }

    #[test]
    fn validate_accepts_valid_entries() {
        let entries = vec![
            entry("docs", "/home/user/docs", AccessMode::ReadOnly),
            entry("build", "/tmp/build", AccessMode::ReadWrite),
        ];
        assert!(validate_entries(&entries).is_ok());
    }

    // -- parse_entries edge cases --------------------------------------------

    #[test]
    fn parse_target_with_space() {
        let text = "myentry /home/user/my documents ro\n";
        let parsed = parse_entries(text).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "myentry");
        assert_eq!(parsed[0].target, PathBuf::from("/home/user/my documents"));
        assert_eq!(parsed[0].mode, AccessMode::ReadOnly);
    }

    #[test]
    fn commented_header_columns_align_with_data_rows() {
        // The `# ` marker must not shift the header relative to the data rows.
        let entries = vec![
            entry("docs", "/tmp/short", AccessMode::ReadOnly),
            entry(
                "a-much-longer-entry-name",
                "/tmp/some/longer/target/path",
                AccessMode::ReadWrite,
            ),
        ];
        let rendered = render_entries(&entries, true);
        let lines: Vec<&str> = rendered.lines().collect();

        // TARGET header sits exactly above the target values; MODE above the modes.
        let header = lines[0];
        let target_col = header.find("TARGET").unwrap();
        let mode_col = header.find("MODE").unwrap();
        for data in &lines[2..] {
            assert_eq!(
                data.find("/tmp/").unwrap(),
                target_col,
                "target column misaligned: {data:?} vs header {header:?}"
            );
            assert!(
                data.ends_with("ro") || data.ends_with("rw"),
                "mode must be the last column: {data:?}"
            );
            // mode is the unpadded final column, so it starts at len - 2.
            assert_eq!(
                data.len() - 2,
                mode_col,
                "mode column misaligned: {data:?} vs header {header:?}"
            );
        }
    }

    #[test]
    fn parse_ignores_comment_and_blank_lines() {
        let text = "\n# ENTRY TARGET MODE\n# ----- ------ ----\n\ndocs /tmp/docs rw\n";
        let parsed = parse_entries(text).expect("should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "docs");
        assert_eq!(parsed[0].mode, AccessMode::ReadWrite);
    }

    #[test]
    fn parse_errors_on_fewer_than_three_tokens() {
        let text = "onlyone\n";
        let err = parse_entries(text).unwrap_err();
        assert!(
            err.to_string().contains("3 tokens"),
            "expected token-count error, got: {err}"
        );

        let text2 = "two tokens\n";
        let err2 = parse_entries(text2).unwrap_err();
        assert!(
            err2.to_string().contains("3 tokens"),
            "expected token-count error, got: {err2}"
        );
    }
}
