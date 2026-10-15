//! Unified diffs as lines the client shows.
use serde::Serialize;

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
}
