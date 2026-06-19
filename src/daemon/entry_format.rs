use crate::{protocol::EntryState, state::AccessMode};

/// Render a slice of entries into a column-aligned status table.
///
/// Columns are in `ENTRY TARGET MODE` order. When `comment_header` is `true`
/// the two header lines are prefixed with `# `; when `false` they are printed
/// bare. Every line ends with `\n`.
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mode_str(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::ReadOnly => "ro",
        AccessMode::ReadWrite => "rw",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str, target: &str, mode: AccessMode) -> EntryState {
        EntryState {
            name: name.to_owned(),
            target: PathBuf::from(target),
            mode,
        }
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
}
