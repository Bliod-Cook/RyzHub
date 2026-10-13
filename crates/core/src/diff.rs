//! Diffs as lines the client shows: a unified diff read as written, or two texts compared.
use serde::Serialize;
use similar::{ChangeTag, TextDiff};

/// Unchanged lines kept around each change of a comparison.
const CONTEXT: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiffLine {
    /// `add`, `del`, `ctx` (unchanged), `hunk` (an `@@` header) or `meta` (file headers and
    /// notes).
    pub kind: &'static str,
    /// The line without its newline, and without the `+`/`-`/space a change line starts with.
    pub text: String,
    /// Its numbers in the old and the new file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<u32>,
}

impl DiffLine {
    fn new(kind: &'static str, text: &str, old: Option<u32>, new: Option<u32>) -> Self {
        DiffLine {
            kind,
            text: text.to_owned(),
            old,
            new,
        }
    }
}

/// Reads a unified diff (`git diff`, `diff -u`). Lines outside hunks are file headers.
pub fn parse(text: &str) -> Vec<DiffLine> {
    let mut out = Vec::new();
    // The next line numbers, and the lines a hunk has left on each side.
    let (mut old, mut new) = (0u32, 0u32);
    let (mut old_left, mut new_left) = (0u32, 0u32);
    for line in text.lines() {
        if old_left == 0 && new_left == 0 {
            if let Some((o, olen, n, nlen)) = hunk_header(line) {
                (old, new, old_left, new_left) = (o, n, olen, nlen);
                out.push(DiffLine::new("hunk", line, None, None));
                continue;
            }
            // Outside hunks: file headers, or changes written without hunk headers, as
            // models often write them.
            let kind = match line.as_bytes().first() {
                _ if line.starts_with("+++ ") || line.starts_with("--- ") => "meta",
                Some(b'+') => "add",
                Some(b'-') => "del",
                Some(b' ') => "ctx",
                _ => "meta",
            };
            let text = if kind == "meta" { line } else { &line[1..] };
            out.push(DiffLine::new(kind, text, None, None));
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                out.push(DiffLine::new("add", &line[1..], None, Some(new)));
                new += 1;
                new_left = new_left.saturating_sub(1);
            }
            Some(b'-') => {
                out.push(DiffLine::new("del", &line[1..], Some(old), None));
                old += 1;
                old_left = old_left.saturating_sub(1);
            }
            Some(b'\\') => out.push(DiffLine::new("meta", line, None, None)),
            _ => {
                let body = line.strip_prefix(' ').unwrap_or(line);
                out.push(DiffLine::new("ctx", body, Some(old), Some(new)));
                old += 1;
                new += 1;
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
            }
        }
    }
    out
}

/// `@@ -a,b +c,d @@`: where each side starts and how many lines it has.
fn hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let side = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let ((o, olen), (n, nlen)) = (side(old)?, side(new)?);
    Some((o, olen, n, nlen))
}

/// What changed from `old` to `new`, line by line, in hunks with a few unchanged lines around
/// each change.
pub fn compare(old: &str, new: &str) -> Vec<DiffLine> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for group in diff.grouped_ops(CONTEXT) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let (o, n) = (first.old_range().start, first.new_range().start);
        let (olen, nlen) = (last.old_range().end - o, last.new_range().end - n);
        out.push(DiffLine::new(
            "hunk",
            &format!("@@ -{},{olen} +{},{nlen} @@", o + 1, n + 1),
            None,
            None,
        ));
        for op in &group {
            for change in diff.iter_changes(op) {
                let text = change.value();
                let text = text.strip_suffix('\n').unwrap_or(text);
                let number = |i: Option<usize>| i.map(|i| i as u32 + 1);
                let (kind, old, new) = match change.tag() {
                    ChangeTag::Equal => (
                        "ctx",
                        number(change.old_index()),
                        number(change.new_index()),
                    ),
                    ChangeTag::Delete => ("del", number(change.old_index()), None),
                    ChangeTag::Insert => ("add", None, number(change.new_index())),
                };
                out.push(DiffLine::new(kind, text, old, new));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diffs_read_with_line_numbers() {
        let lines = parse(
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -2,3 +2,3 @@ fn\n a\n-b\n+c\n d\n\\ No newline at end of file\n",
        );
        let kinds: Vec<_> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                "meta", "meta", "meta", "hunk", "ctx", "del", "add", "ctx", "meta"
            ]
        );
        assert_eq!(lines[5], DiffLine::new("del", "b", Some(3), None));
        assert_eq!(lines[6], DiffLine::new("add", "c", None, Some(3)));
        assert_eq!(lines[7], DiffLine::new("ctx", "d", Some(4), Some(4)));
        // Changes written without a hunk header still read as changes.
        let bare: Vec<_> = parse("-a\n+b\n c").iter().map(|l| l.kind).collect();
        assert_eq!(bare, ["del", "add", "ctx"]);
    }

    #[test]
    fn texts_compare_into_hunks() {
        let old = (1..=20).map(|i| format!("{i}\n")).collect::<String>();
        let new = old.replace("10\n", "ten\n");
        let lines = compare(&old, &new);
        assert_eq!(lines[0].text, "@@ -7,7 +7,7 @@");
        assert!(lines.contains(&DiffLine::new("del", "10", Some(10), None)));
        assert!(lines.contains(&DiffLine::new("add", "ten", None, Some(10))));
        assert_eq!(lines.len(), 1 + 3 + 2 + 3);
    }
}
