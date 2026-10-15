//! Markdown as the timeline shows it: blocks of styled inlines, parsed in the kernel.
//!
//! A message renders to [`Block`]s. While it streams, [`Doc`] keeps the blocks it has finished
//! frozen and parses only what follows them, so each batch costs about its last block. Before
//! parsing, TeX (`$…$`, `$$…$$`, `\(…\)`, `\[…\]`) and runs of cited source links become
//! placeholders, and syntax a streaming message has left open at its end is closed.
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use std::ops::Range;

/// [`Inline::Text`] styles.
pub const STRONG: u8 = 1;
pub const EMPHASIS: u8 = 2;
pub const STRIKE: u8 = 4;

/// Placeholders the pre-pass leaves for a citation chip and a formula, each around its number:
/// private-use code points that no reply contains.
const CHIP: (char, char) = ('\u{E000}', '\u{E001}');
const MATH: (char, char) = ('\u{E002}', '\u{E003}');

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Block {
    Paragraph {
        inlines: Vec<Inline>,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    /// A code block: its info string as fenced, empty when indented.
    Code {
        info: String,
        text: String,
    },
    /// A formula displayed on lines of its own.
    Math {
        tex: String,
        source: String,
    },
    Quote {
        blocks: Vec<Block>,
    },
    List {
        /// The first number of an ordered list.
        #[serde(skip_serializing_if = "Option::is_none")]
        start: Option<u64>,
        tight: bool,
        items: Vec<ListItem>,
    },
    Table {
        /// Each column's alignment: "left", "center", "right" or "".
        align: Vec<&'static str>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
    /// HTML, shown as written.
    Html {
        text: String,
    },
    /// A footnote's definition.
    Note {
        label: String,
        blocks: Vec<Block>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ListItem {
    /// A task list item's checkbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Inline {
    Text {
        text: String,
        /// [`STRONG`], [`EMPHASIS`] and [`STRIKE`].
        #[serde(skip_serializing_if = "is_zero")]
        style: u8,
        /// The link the text is part of.
        #[serde(skip_serializing_if = "Option::is_none")]
        href: Option<String>,
    },
    Code {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        href: Option<String>,
    },
    /// TeX: inline, or displayed on a line of its own within the paragraph.
    Math {
        tex: String,
        /// As written, shown when the TeX does not parse.
        source: String,
        display: bool,
    },
    /// A run of cited sources, by its index in [`Rendered::chips`].
    Chip {
        index: usize,
    },
    /// A link to a file of the sandbox: `sandbox:/workspace/…` or `/attachments/…`. `path`
    /// is the link as the server records the files a reply links to: without `sandbox:` and
    /// percent-decoded.
    File {
        label: String,
        path: String,
    },
    /// An image, shown as a link: a remote image loads only when the user opens it.
    Image {
        alt: String,
        url: String,
    },
    Break,
}

fn is_zero(style: &u8) -> bool {
    *style == 0
}

/// A cited source a chip stands for: its link's label and URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Source {
    pub label: String,
    pub url: String,
}

/// A message's blocks, and the citation chips they show.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Rendered {
    pub blocks: Vec<Block>,
    pub chips: Vec<Vec<Source>>,
}

/// What changed since the last render: blocks from `from` on are new.
#[derive(Clone, Debug, PartialEq)]
pub struct Splice {
    pub from: usize,
    pub chips_changed: bool,
}

/// Renders `text` whole. `cited` are the URLs of the pages the message cites; `streaming`
/// closes what its end leaves open and hides an unfinished run of source links.
pub fn render(text: &str, cited: &[String], streaming: bool) -> Rendered {
    let mut doc = Doc {
        freezing: false,
        ..Default::default()
    };
    doc.update(text, cited, streaming);
    doc.rendered()
}

/// A message rendered as it streams. Blocks followed by a block whose first line is complete
/// are frozen: they cannot change as text arrives, so only what follows is parsed again.
#[derive(Clone, Debug)]
pub struct Doc {
    freezing: bool,
    /// The cited URLs the frozen blocks were folded with.
    cited: Vec<String>,
    /// The source of the frozen blocks.
    frozen_source: String,
    frozen: Rendered,
    /// The blocks and chips after the frozen ones, as last rendered.
    tail: Rendered,
}

impl Default for Doc {
    fn default() -> Self {
        Doc {
            freezing: true,
            cited: vec![],
            frozen_source: String::new(),
            frozen: Rendered::default(),
            tail: Rendered::default(),
        }
    }
}

impl Doc {
    /// Renders `text`, the whole message so far. A message that is not its last text extended
    /// (replaced, or citing other pages) renders afresh.
    pub fn update(&mut self, text: &str, cited: &[String], streaming: bool) -> Splice {
        let old_frozen = self.frozen.blocks.len();
        let old_chips: Vec<Vec<Source>> = self.chips().cloned().collect();
        let old_tail = std::mem::take(&mut self.tail);
        let mut afresh = false;
        if cited != self.cited || !text.starts_with(self.frozen_source.as_str()) {
            self.cited = cited.to_vec();
            self.unfreeze();
            afresh = true;
        }
        let base = self.frozen_source.len();
        let mut tail = render_tail(
            &text[base..],
            cited,
            streaming,
            self.frozen.chips.len(),
            self.freezing,
        );
        if tail.nonlocal && base > 0 {
            // A reference or footnote definition may change any block: no block is final.
            self.unfreeze();
            afresh = true;
            tail = render_tail(text, cited, streaming, 0, false);
        }
        let newly_frozen = self.frozen.blocks.len();
        if let Some(freeze) = tail.freeze.filter(|_| !tail.nonlocal) {
            let start = self.frozen_source.len();
            self.frozen_source
                .push_str(&text[start..start + freeze.source]);
            self.frozen
                .blocks
                .extend(tail.rendered.blocks.drain(..freeze.blocks));
            self.frozen
                .chips
                .extend(tail.rendered.chips.drain(..freeze.chips));
        }
        self.tail = tail.rendered;
        // Frozen blocks are unchanged unless the message rendered afresh.
        let from = if afresh {
            0
        } else {
            let same = self.frozen.blocks[newly_frozen..]
                .iter()
                .chain(&self.tail.blocks)
                .zip(&old_tail.blocks)
                .take_while(|(a, b)| a == b)
                .count();
            old_frozen + same
        };
        Splice {
            from,
            chips_changed: self.chips().ne(old_chips.iter()),
        }
    }

    fn unfreeze(&mut self) {
        self.frozen_source.clear();
        self.frozen = Rendered::default();
    }

    /// Every block, frozen and not.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.frozen.blocks.iter().chain(&self.tail.blocks)
    }

    pub fn len(&self) -> usize {
        self.frozen.blocks.len() + self.tail.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn chips(&self) -> impl Iterator<Item = &Vec<Source>> {
        self.frozen.chips.iter().chain(&self.tail.chips)
    }

    pub fn rendered(&self) -> Rendered {
        Rendered {
            blocks: self.blocks().cloned().collect(),
            chips: self.chips().cloned().collect(),
        }
    }
}

/// Where a rendered tail may be frozen: the length of its source, of its blocks and of its
/// chips before that point.
#[derive(Clone, Copy, Debug)]
struct Freeze {
    source: usize,
    blocks: usize,
    chips: usize,
}

struct Tail {
    rendered: Rendered,
    freeze: Option<Freeze>,
    /// It defines a link reference or footnote, which blocks elsewhere may use.
    nonlocal: bool,
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}

fn render_tail(
    src: &str,
    cited: &[String],
    streaming: bool,
    chip_base: usize,
    freezing: bool,
) -> Tail {
    let outline = outline(src);
    let mut edits = Vec::new();
    let mut chips = Vec::new();
    fold(
        src,
        &outline.code,
        cited,
        streaming,
        chip_base,
        &mut edits,
        &mut chips,
    );
    let mut skip: Vec<Range<usize>> = outline
        .code
        .iter()
        .chain(&outline.destinations)
        .cloned()
        .chain(edits.iter().map(|e: &Edit| e.range.clone()))
        .collect();
    skip.sort_by_key(|r| (r.start, r.end));
    let mut formulas = Vec::new();
    let unclosed = maths(src, &skip, &mut edits, &mut formulas);
    edits.sort_by_key(|e| e.range.start);
    let mut text = apply(src, &edits);
    let last = outline.starts.last().copied().unwrap_or(0);
    if streaming && !outline.verbatim_last && !inside(&edits, last) {
        complete(&mut text, shift(&edits, last));
    }
    let (blocks, starts) = build(&text, &formulas);
    // A block whose first line is complete has closed the blocks before it, unless a formula
    // spans them or one left open may come to.
    let complete_lines = src.rfind('\n').map_or(0, |n| n + 1);
    let freeze = outline
        .starts
        .iter()
        .skip(1)
        .rev()
        .filter(|&&b| {
            freezing
                && !outline.nonlocal
                && b < complete_lines
                && !inside(&edits, b)
                && unclosed.is_none_or(|u| b <= u)
        })
        .find_map(|&b| {
            let blocks = starts.binary_search(&shift(&edits, b)).ok()?;
            (blocks > 0).then(|| Freeze {
                source: b,
                blocks,
                chips: edits.iter().filter(|e| e.chip && e.range.start < b).count(),
            })
        });
    Tail {
        rendered: Rendered { blocks, chips },
        freeze,
        nonlocal: outline.nonlocal,
    }
}

/// What the first parse finds: where top-level blocks start and what the pre-pass must not
/// read in.
#[derive(Default)]
struct Outline {
    /// Each top-level block's start, at the start of its first line.
    starts: Vec<usize>,
    /// The last top-level block is code or HTML, which nothing closes.
    verbatim_last: bool,
    /// Code blocks and spans.
    code: Vec<Range<usize>>,
    /// Link destinations.
    destinations: Vec<Range<usize>>,
    nonlocal: bool,
}

fn outline(src: &str) -> Outline {
    let mut out = Outline::default();
    let mut depth = 0usize;
    // Open links: where their label's text ends so far.
    let mut links: Vec<usize> = Vec::new();
    let parser = Parser::new_ext(src, options());
    out.nonlocal = parser.reference_definitions().iter().next().is_some();
    for (event, range) in parser.into_offset_iter() {
        if let Some(label) = links.last_mut()
            && !matches!(event, Event::End(TagEnd::Link | TagEnd::Image))
        {
            *label = (*label).max(range.end);
        }
        match &event {
            Event::Start(tag) => {
                if depth == 0 {
                    out.starts.push(line_start(src, range.start));
                    out.verbatim_last = matches!(tag, Tag::CodeBlock(_) | Tag::HtmlBlock);
                }
                match tag {
                    Tag::CodeBlock(_) | Tag::HtmlBlock => out.code.push(range.clone()),
                    Tag::Link { .. } | Tag::Image { .. } => links.push(range.start + 1),
                    Tag::FootnoteDefinition(_) => out.nonlocal = true,
                    _ => {}
                }
                depth += 1;
            }
            Event::End(end) => {
                depth = depth.saturating_sub(1);
                if matches!(end, TagEnd::Link | TagEnd::Image)
                    && let Some(label) = links.pop()
                {
                    out.destinations.push(label.min(range.end)..range.end);
                }
            }
            Event::Code(_) => out.code.push(range.clone()),
            Event::Rule | Event::Html(_) if depth == 0 => {
                out.starts.push(line_start(src, range.start));
                out.verbatim_last = matches!(event, Event::Html(_));
            }
            Event::FootnoteReference(_) => out.nonlocal = true,
            _ => {}
        }
    }
    out.code.sort_by_key(|r| r.start);
    out
}

fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// A change the pre-pass makes to the source.
struct Edit {
    range: Range<usize>,
    with: String,
    chip: bool,
}

fn apply(src: &str, edits: &[Edit]) -> String {
    let mut out = String::with_capacity(src.len());
    let mut cursor = 0;
    for edit in edits {
        out.push_str(&src[cursor..edit.range.start]);
        out.push_str(&edit.with);
        cursor = edit.range.end;
    }
    out.push_str(&src[cursor..]);
    out
}

/// Where `at` in the source lands in the edited text; `at` is not inside an edit.
fn shift(edits: &[Edit], at: usize) -> usize {
    let mut out = at as isize;
    for edit in edits.iter().filter(|e| e.range.end <= at) {
        out += edit.with.len() as isize - edit.range.len() as isize;
    }
    out as usize
}

fn inside(edits: &[Edit], at: usize) -> bool {
    edits
        .iter()
        .any(|e| e.range.start < at && at < e.range.end.max(e.range.start + 1))
}

/// The range of `ranges` (sorted by start) holding `at`, if any.
fn covering(ranges: &[Range<usize>], at: usize) -> Option<&Range<usize>> {
    ranges.iter().find(|r| r.start <= at && at < r.end)
}

/// Runs of source links to cited pages become chips. Models write a passage's sources after it
/// as `([site](url))`; groups apart by spaces only read as one. At the end of a streaming
/// message, a run not yet finished, or not yet cited, is hidden.
fn fold(
    src: &str,
    code: &[Range<usize>],
    cited: &[String],
    streaming: bool,
    chip_base: usize,
    edits: &mut Vec<Edit>,
    chips: &mut Vec<Vec<Source>>,
) {
    let is_cited = |links: &[(Range<usize>, Range<usize>)]| {
        links
            .iter()
            .all(|(_, url)| cited.iter().any(|c| c == &src[url.clone()]))
    };
    let mut at = 0;
    while let Some(found) = src[at..].find("([") {
        at += found;
        if let Some(range) = covering(code, at) {
            at = range.end;
            continue;
        }
        let first = group(src, at);
        let complete = first
            .as_ref()
            .and_then(|(end, links)| end.map(|e| (e, links)));
        match complete {
            Some((end, links)) if !cited.is_empty() && is_cited(links) => {
                let (mut end, mut links) = (end, links.clone());
                loop {
                    let next = end + src[end..].len() - src[end..].trim_start_matches(' ').len();
                    match group(src, next) {
                        Some((Some(more_end), more)) if is_cited(&more) => {
                            end = more_end;
                            links.extend(more);
                        }
                        _ => break,
                    }
                }
                let mut sources: Vec<Source> = Vec::new();
                for (label, url) in links {
                    let url = &src[url];
                    if !sources.iter().any(|s| s.url == url) {
                        sources.push(Source {
                            label: src[label].to_owned(),
                            url: url.to_owned(),
                        });
                    }
                }
                let index = chip_base + chips.len();
                chips.push(sources);
                edits.push(Edit {
                    range: at..end,
                    with: format!("{}{index}{}", CHIP.0, CHIP.1),
                    chip: true,
                });
                at = end;
            }
            _ => {
                // At the end of a streaming message, links wait to be finished and cited.
                let waiting = first
                    .as_ref()
                    .is_some_and(|(end, _)| end.is_none_or(|e| src[e..].trim().is_empty()));
                if waiting && streaming {
                    edits.push(Edit {
                        range: at..src.len(),
                        with: String::new(),
                        chip: false,
                    });
                    return;
                }
                at += 1;
            }
        }
    }
}

/// The links of a `([label](url), [label](url))` at `start`: each as the ranges of its label
/// and URL, and the group's end, None when the source stops inside it.
type Links = Vec<(Range<usize>, Range<usize>)>;
fn group(src: &str, start: usize) -> Option<(Option<usize>, Links)> {
    if !src[start..].starts_with("([") {
        return None;
    }
    let bytes = src.as_bytes();
    let mut links = Vec::new();
    let mut i = start + 1;
    loop {
        if i >= bytes.len() {
            return Some((None, vec![]));
        }
        if bytes[i] != b'[' {
            return None;
        }
        let close = src[i..].find("](").map(|c| c + i);
        let line = src[i..].find('\n').map(|l| l + i);
        let Some(close) = close else {
            return line.is_none().then(|| (None, vec![]));
        };
        if line.is_some_and(|l| l < close) {
            return None;
        }
        // The link ends at the parenthesis that closes it; ones in the URL pair up.
        let mut depth = 0i32;
        let mut j = close + 2;
        while j < bytes.len() {
            match bytes[j] {
                b' ' | b'\n' => return None,
                b'(' => depth += 1,
                b')' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            j += 1;
        }
        if j >= bytes.len() {
            return Some((None, vec![]));
        }
        let url = close + 2..j;
        if !src[url.clone()].starts_with("https://") && !src[url.clone()].starts_with("http://") {
            return None;
        }
        links.push((i + 1..close, url));
        i = j + 1;
        while let Some(c) = src[i..].chars().next()
            && ", ;，；".contains(c)
        {
            i += c.len_utf8();
        }
        if bytes.get(i) == Some(&b')') {
            return Some((Some(i + 1), links));
        }
    }
}

/// A formula the pre-pass took out of the source.
#[derive(Clone, Debug)]
struct Formula {
    tex: String,
    source: String,
    display: bool,
}

/// TeX becomes placeholders: `$$…$$` and `\[…\]` displayed, `\(…\)` and `$…$` inline. A `$`
/// pair follows Pandoc so prices stay text: no space inside either `$`, no digit after the
/// closing one, on one line. A displayed formula opened at a line's start may span blank
/// lines. Returns where the first displayed formula left open begins.
fn maths(
    src: &str,
    skip: &[Range<usize>],
    edits: &mut Vec<Edit>,
    formulas: &mut Vec<Formula>,
) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut unclosed = None;
    let mut i = 0;
    // The first span left alone that does not end before `i`.
    let mut k = 0;
    while i < bytes.len() {
        while k < skip.len() && skip[k].end <= i {
            k += 1;
        }
        if let Some(range) = skip.get(k).filter(|r| r.start <= i) {
            i = range.end;
            continue;
        }
        // Formulas end before the next span left alone.
        let limit = skip.get(k).map_or(bytes.len(), |r| r.start);
        let found = match bytes[i] {
            b'$' if bytes.get(i + 1) == Some(&b'$') => {
                let found = displayed(src, i, "$$", "$$", limit);
                if found.is_none() && src[i + 2..].find("$$").is_none() {
                    unclosed = unclosed.or(Some(i));
                }
                found
            }
            b'\\' => match bytes.get(i + 1) {
                Some(b'[') => {
                    let found = displayed(src, i, "\\[", "\\]", limit);
                    if found.is_none() && src[i + 2..].find("\\]").is_none() {
                        unclosed = unclosed.or(Some(i));
                    }
                    found
                }
                Some(b'(') if i + 2 <= limit => {
                    let close = src[i + 2..limit].find("\\)").map(|c| c + i + 2);
                    close
                        .filter(|&c| c > i + 2 && !blank_line(&src[i..c]))
                        .map(|c| (c + 2, i + 2..c, false))
                }
                Some(c) if c.is_ascii_punctuation() => {
                    i += 2;
                    continue;
                }
                _ => None,
            },
            b'$' => dollar(src, i, limit).map(|end| (end + 1, i + 1..end, false)),
            _ => None,
        };
        match found {
            Some((end, tex, display)) => {
                let index = formulas.len();
                formulas.push(Formula {
                    tex: src[tex].to_owned(),
                    source: src[i..end].to_owned(),
                    display,
                });
                edits.push(Edit {
                    range: i..end,
                    with: format!("{}{index}{}", MATH.0, MATH.1),
                    chip: false,
                });
                i = end;
            }
            None => i += 1,
        }
    }
    unclosed
}

/// A displayed formula opened at `i`: its end, its TeX and that it is displayed.
fn displayed(
    src: &str,
    i: usize,
    open: &str,
    close: &str,
    limit: usize,
) -> Option<(usize, Range<usize>, bool)> {
    let from = i + open.len();
    // At least one character of TeX.
    let first = src.get(from..limit)?.chars().next()?.len_utf8();
    let at = src[from + first..limit].find(close)? + from + first;
    let tex = from..at;
    if src[tex.clone()].trim().is_empty() {
        return None;
    }
    let end = at + close.len();
    if blank_line(&src[i..end]) {
        // Across blank lines only as a block: opened at a line's start, closed at a line's end.
        let line = line_start(src, i);
        let indent = &src[line..i];
        let rest = src[end..].split('\n').next().unwrap_or("");
        if indent.len() > 3 || !indent.chars().all(|c| c == ' ') || !rest.trim().is_empty() {
            return None;
        }
    }
    Some((end, tex, true))
}

fn blank_line(s: &str) -> bool {
    s.split('\n').skip(1).any(|line| line.trim().is_empty()) && s.matches('\n').count() >= 2
}

/// The closing `$` of an inline formula opened at `i`, Pandoc style.
fn dollar(src: &str, i: usize, limit: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let next = src[i + 1..limit].chars().next()?;
    if next.is_whitespace() || next == '$' {
        return None;
    }
    let mut j = i + 1;
    while j < limit {
        match bytes[j] {
            b'\\' => {
                let escaped = src[j + 1..limit].chars().next()?;
                if escaped == '\n' {
                    return None;
                }
                j += 1 + escaped.len_utf8();
            }
            b'\n' => return None,
            b'$' => {
                let before = src[..j].chars().next_back()?;
                let after_digit = bytes.get(j + 1).is_some_and(u8::is_ascii_digit);
                return (j > i + 1 && !before.is_whitespace() && !after_digit).then_some(j);
            }
            _ => j += 1,
        }
    }
    None
}

/// Closes what the end of a streaming message leaves open, from `from` (its last block) on: a
/// link whose destination is still coming shows its label alone, and code spans, strong,
/// emphasis and strikethrough are closed. A marker with nothing after it yet is dropped.
fn complete(text: &mut String, from: usize) {
    if let Some(at) = text[from..].rfind("](").map(|a| a + from) {
        let rest = &text[at + 2..];
        if !rest.contains(')')
            && !rest.contains(char::is_whitespace)
            && let Some(open) = text[from..at].rfind('[').map(|o| o + from)
            // Not inside a code span.
            && text[from..open].matches('`').count().is_multiple_of(2)
        {
            let image = open > from && text.as_bytes()[open - 1] == b'!';
            let label = text[open + 1..at].to_owned();
            text.truncate(if image { open - 1 } else { open });
            if !image {
                text.push_str(&label);
            }
        }
    }
    let bytes = text.as_bytes();
    let mut ticks: Option<usize> = None;
    let mut open: Vec<&'static str> = Vec::new();
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'`' {
            let run = bytes[i..].iter().take_while(|&&c| c == b'`').count();
            ticks = match ticks {
                Some(n) if n == run => None,
                None => Some(run),
                other => other,
            };
            i += run;
            continue;
        }
        if ticks.is_some() {
            i += 1;
            continue;
        }
        if b == b'\\' {
            i += 2;
            continue;
        }
        let marker = if bytes[i..].starts_with(b"**") {
            Some("**")
        } else if bytes[i..].starts_with(b"~~") {
            Some("~~")
        } else if b == b'*' {
            let before = text[..i].chars().next_back();
            let after = text[i + 1..].chars().next();
            let spaced = |c: Option<char>| c.is_none_or(char::is_whitespace);
            // A bullet or a product, not emphasis.
            (!(spaced(before) && spaced(after))).then_some("*")
        } else {
            None
        };
        if let Some(marker) = marker {
            if open.last() == Some(&marker) {
                open.pop();
            } else {
                open.push(marker);
            }
            i += marker.len();
            continue;
        }
        i += 1;
    }
    let mut suffix = String::new();
    if let Some(n) = ticks {
        suffix.push_str(&"`".repeat(n));
    }
    for marker in open.iter().rev() {
        let trimmed = text.trim_end().len();
        if suffix.is_empty() && text[..trimmed].ends_with(marker) {
            text.truncate(trimmed - marker.len());
        } else {
            suffix.push_str(marker);
        }
    }
    text.push_str(&suffix);
}

/// The path of the sandbox file `href` names, as [`Inline::File`] carries it; None for a page.
fn file_path(href: &str) -> Option<String> {
    let path = percent_decode(href.strip_prefix("sandbox:").unwrap_or(href));
    (path.starts_with("/workspace/") || path.starts_with("/attachments/")).then_some(path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Unchanged unless decoded it is UTF-8, as the server reads it.
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// A container being built.
enum Node {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    Note(String, Vec<Block>),
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<ListItem>,
    },
    /// A list item: its blocks, and the text of a tight item not yet in a paragraph.
    Item {
        task: Option<bool>,
        blocks: Vec<Block>,
        inlines: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    Code(String, String),
    Html(String),
    Table {
        align: Vec<&'static str>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        row: Vec<Vec<Inline>>,
    },
    Cell(Vec<Inline>),
}

struct Builder<'a> {
    formulas: &'a [Formula],
    stack: Vec<Node>,
    strong: u32,
    emphasis: u32,
    strike: u32,
    /// Open links: the destination, the sandbox file it names, and where its text starts in
    /// the inlines of the node holding it.
    links: Vec<(String, Option<String>, usize)>,
    /// An image being read: its URL and alt text.
    image: Option<(String, String)>,
}

/// The blocks of `text`, and where each starts (at its first line).
fn build(text: &str, formulas: &[Formula]) -> (Vec<Block>, Vec<usize>) {
    let mut b = Builder {
        formulas,
        stack: vec![Node::Root(vec![])],
        strong: 0,
        emphasis: 0,
        strike: 0,
        links: vec![],
        image: None,
    };
    let mut starts = Vec::new();
    for (event, range) in Parser::new_ext(text, options()).into_offset_iter() {
        if b.stack.len() == 1 && matches!(event, Event::Start(_) | Event::Rule) {
            starts.push(line_start(text, range.start));
        }
        b.event(event);
    }
    let Some(Node::Root(blocks)) = b.stack.pop() else {
        unreachable!("the root stays at the bottom");
    };
    (blocks, starts)
}

impl Builder<'_> {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(end) => self.end(end),
            Event::Text(text) => {
                if self.image.is_some() {
                    let restored = self.restore(&text);
                    if let Some((_, alt)) = &mut self.image {
                        alt.push_str(&restored);
                    }
                } else if let Some(Node::Code(_, code) | Node::Html(code)) = self.stack.last_mut() {
                    code.push_str(&text);
                } else {
                    self.text(&text);
                }
            }
            Event::Code(text) => {
                let href = self.href();
                self.inline(Inline::Code {
                    text: text.into_string(),
                    href,
                });
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if let Some(Node::Html(code)) = self.stack.last_mut() {
                    code.push_str(&html);
                } else {
                    self.plain(&html);
                }
            }
            Event::InlineMath(tex) | Event::DisplayMath(tex) => self.plain(&tex),
            Event::FootnoteReference(label) => self.plain(&format!("[{label}]")),
            Event::SoftBreak => {
                if let Some((_, alt)) = &mut self.image {
                    alt.push(' ');
                } else {
                    self.plain(" ");
                }
            }
            Event::HardBreak => self.inline(Inline::Break),
            Event::Rule => self.block(Block::Rule),
            Event::TaskListMarker(done) => {
                if let Some(Node::Item { task, .. }) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        let node = match tag {
            Tag::Paragraph => {
                // A paragraph in an item makes its list loose.
                if matches!(self.stack.last(), Some(Node::Item { .. })) {
                    self.flush_item();
                    if let Some(Node::List { tight, .. }) = self.stack.iter_mut().rev().nth(1) {
                        *tight = false;
                    }
                }
                Node::Paragraph(vec![])
            }
            Tag::Heading { level, .. } => Node::Heading(level as u8, vec![]),
            Tag::BlockQuote(_) => Node::Quote(vec![]),
            Tag::CodeBlock(kind) => Node::Code(
                match kind {
                    CodeBlockKind::Fenced(info) => info.into_string(),
                    CodeBlockKind::Indented => String::new(),
                },
                String::new(),
            ),
            Tag::HtmlBlock => Node::Html(String::new()),
            Tag::List(start) => Node::List {
                start,
                tight: true,
                items: vec![],
            },
            Tag::Item => Node::Item {
                task: None,
                blocks: vec![],
                inlines: vec![],
            },
            Tag::FootnoteDefinition(label) => Node::Note(label.into_string(), vec![]),
            Tag::Table(align) => Node::Table {
                align: align
                    .iter()
                    .map(|a| match a {
                        Alignment::Left => "left",
                        Alignment::Center => "center",
                        Alignment::Right => "right",
                        Alignment::None => "",
                    })
                    .collect(),
                head: vec![],
                rows: vec![],
                row: vec![],
            },
            Tag::TableHead | Tag::TableRow => return,
            Tag::TableCell => Node::Cell(vec![]),
            Tag::Emphasis => return self.emphasis += 1,
            Tag::Strong => return self.strong += 1,
            Tag::Strikethrough => return self.strike += 1,
            Tag::Link { dest_url, .. } => {
                let file = file_path(&dest_url);
                let at = self.inlines().map_or(0, |i| i.len());
                self.links.push((dest_url.into_string(), file, at));
                return;
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some((dest_url.into_string(), String::new()));
                return;
            }
            Tag::Superscript
            | Tag::Subscript
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => return,
        };
        self.stack.push(node);
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                let Some((_, file, at)) = self.links.pop() else {
                    return;
                };
                if let Some(path) = file
                    && let Some(inlines) = self.inlines()
                {
                    let label: String = inlines
                        .drain(at.min(inlines.len())..)
                        .map(text_of)
                        .collect();
                    inlines.push(Inline::File { label, path });
                }
            }
            TagEnd::Image => {
                if let Some((url, alt)) = self.image.take() {
                    self.inline(match file_path(&url) {
                        Some(path) => Inline::File {
                            label: if alt.is_empty() { path.clone() } else { alt },
                            path,
                        },
                        None => Inline::Image { alt, url },
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(Node::Table { head, row, .. }) = self.stack.last_mut() {
                    *head = std::mem::take(row);
                }
            }
            TagEnd::TableRow => {
                if let Some(Node::Table { rows, row, .. }) = self.stack.last_mut() {
                    rows.push(std::mem::take(row));
                }
            }
            TagEnd::TableCell => {
                if let Some(Node::Cell(cell)) = self.stack.pop()
                    && let Some(Node::Table { row, .. }) = self.stack.last_mut()
                {
                    row.push(linkified(cell));
                }
            }
            TagEnd::Item => {
                self.flush_item();
                if let Some(Node::Item { task, blocks, .. }) = self.stack.pop()
                    && let Some(Node::List { items, .. }) = self.stack.last_mut()
                {
                    items.push(ListItem { task, blocks });
                }
            }
            TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::Table => {
                let block = match self.stack.pop() {
                    Some(Node::Paragraph(inlines)) => paragraph(linkified(inlines)),
                    Some(Node::Heading(level, inlines)) => Block::Heading {
                        level,
                        inlines: linkified(inlines),
                    },
                    Some(Node::Quote(blocks)) => Block::Quote { blocks },
                    Some(Node::Code(info, text)) => Block::Code { info, text },
                    Some(Node::Html(text)) => Block::Html { text },
                    Some(Node::List {
                        start,
                        tight,
                        items,
                    }) => Block::List {
                        start,
                        tight,
                        items,
                    },
                    Some(Node::Note(label, blocks)) => Block::Note { label, blocks },
                    Some(Node::Table {
                        align, head, rows, ..
                    }) => Block::Table { align, head, rows },
                    Some(other) => {
                        self.stack.push(other);
                        return;
                    }
                    None => return,
                };
                self.block(block);
            }
        }
    }

    /// The text of a tight list item becomes a paragraph before a block follows it.
    fn flush_item(&mut self) {
        if let Some(Node::Item {
            blocks, inlines, ..
        }) = self.stack.last_mut()
            && !inlines.is_empty()
        {
            blocks.push(paragraph(linkified(std::mem::take(inlines))));
        }
    }

    fn block(&mut self, block: Block) {
        self.flush_item();
        if let Some(
            Node::Root(blocks)
            | Node::Quote(blocks)
            | Node::Note(_, blocks)
            | Node::Item { blocks, .. },
        ) = self.stack.last_mut()
        {
            blocks.push(block);
        }
    }

    fn inlines(&mut self) -> Option<&mut Vec<Inline>> {
        match self.stack.last_mut()? {
            Node::Paragraph(inlines)
            | Node::Heading(_, inlines)
            | Node::Cell(inlines)
            | Node::Item { inlines, .. } => Some(inlines),
            _ => None,
        }
    }

    fn inline(&mut self, inline: Inline) {
        if let Some(inlines) = self.inlines() {
            inlines.push(inline);
        } else {
            // Text where no block holds it reads as a paragraph of its own.
            self.block(Block::Paragraph {
                inlines: vec![inline],
            });
        }
    }

    fn style(&self) -> u8 {
        (if self.strong > 0 { STRONG } else { 0 })
            | (if self.emphasis > 0 { EMPHASIS } else { 0 })
            | (if self.strike > 0 { STRIKE } else { 0 })
    }

    fn href(&self) -> Option<String> {
        self.links.last().map(|(href, ..)| href.clone())
    }

    /// Text as it is, joined to the text before it when styled alike.
    fn plain(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let (style, href) = (self.style(), self.href());
        if let Some(inlines) = self.inlines()
            && let Some(Inline::Text {
                text: last,
                style: s,
                href: h,
            }) = inlines.last_mut()
            && *s == style
            && *h == href
        {
            last.push_str(text);
            return;
        }
        self.inline(Inline::Text {
            text: text.to_owned(),
            style,
            href,
        });
    }

    /// Text with its placeholders back as chips and formulas, and its bare web addresses as
    /// links.
    fn text(&mut self, text: &str) {
        let mut rest = text;
        while let Some(at) = rest.find([CHIP.0, MATH.0]) {
            let open = rest[at..].chars().next().unwrap();
            let close = if open == CHIP.0 { CHIP.1 } else { MATH.1 };
            let body = &rest[at + open.len_utf8()..];
            let Some(end) = body.find(close) else {
                break;
            };
            let Ok(index) = body[..end].parse::<usize>() else {
                break;
            };
            self.plain(&rest[..at]);
            if open == CHIP.0 {
                self.inline(Inline::Chip { index });
            } else if let Some(formula) = self.formulas.get(index) {
                self.inline(Inline::Math {
                    tex: formula.tex.clone(),
                    source: formula.source.clone(),
                    display: formula.display,
                });
            }
            rest = &body[end + close.len_utf8()..];
        }
        if !rest.is_empty() {
            self.plain(rest);
        }
    }

    /// Text with its placeholders back as written.
    fn restore(&self, text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find(MATH.0) {
            let body = &rest[at + MATH.0.len_utf8()..];
            let Some(end) = body.find(MATH.1) else {
                break;
            };
            out.push_str(&rest[..at]);
            if let Some(formula) = body[..end]
                .parse::<usize>()
                .ok()
                .and_then(|i| self.formulas.get(i))
            {
                out.push_str(&formula.source);
            }
            rest = &body[end + MATH.1.len_utf8()..];
        }
        out.push_str(rest);
        out.replace([CHIP.0, CHIP.1], "")
    }
}

/// `inlines` with the bare web addresses in their plain text as links.
fn linkified(inlines: Vec<Inline>) -> Vec<Inline> {
    let mut out = Vec::with_capacity(inlines.len());
    for inline in inlines {
        let Inline::Text {
            text,
            style,
            href: None,
        } = &inline
        else {
            out.push(inline);
            continue;
        };
        let links = autolinks(text);
        if links.is_empty() {
            out.push(inline);
            continue;
        }
        let mut cursor = 0;
        let mut piece = |range: Range<usize>, href: Option<String>| {
            if !range.is_empty() {
                out.push(Inline::Text {
                    text: text[range].to_owned(),
                    style: *style,
                    href,
                });
            }
        };
        for (range, href) in links {
            piece(cursor..range.start, None);
            cursor = range.end;
            piece(range, Some(href));
        }
        piece(cursor..text.len(), None);
    }
    out
}

fn text_of(inline: Inline) -> String {
    match inline {
        Inline::Text { text, .. } | Inline::Code { text, .. } => text,
        Inline::Math { source, .. } => source,
        Inline::File { label, .. } => label,
        Inline::Image { alt, .. } => alt,
        Inline::Break => "\n".into(),
        Inline::Chip { .. } => String::new(),
    }
}

/// A paragraph of one displayed formula is that formula's block.
fn paragraph(inlines: Vec<Inline>) -> Block {
    let mut content = inlines
        .iter()
        .filter(|i| !matches!(i, Inline::Text { text, .. } if text.trim().is_empty()));
    if let (
        Some(Inline::Math {
            tex,
            source,
            display: true,
        }),
        None,
    ) = (content.next(), content.next())
    {
        return Block::Math {
            tex: tex.clone(),
            source: source.clone(),
        };
    }
    Block::Paragraph { inlines }
}

/// Bare web addresses in `text` (`https://…`, `http://…`, `www.…`), with what each links to.
/// An address ends at a space or `<`, less trailing punctuation and unpaired `)`.
fn autolinks(text: &str) -> Vec<(Range<usize>, String)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let Some(found) = ["https://", "http://", "www."]
            .iter()
            .filter_map(|p| rest.find(p).map(|i| (i, *p)))
            .min_by_key(|(i, _)| *i)
        else {
            break;
        };
        let start = at + found.0;
        let boundary = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || "*_~(\"'".contains(c));
        let mut end = start
            + text[start..]
                .find(|c: char| c.is_whitespace() || c == '<')
                .unwrap_or(text.len() - start);
        loop {
            let tail = text[start..end].chars().next_back();
            match tail {
                Some('?' | '!' | '.' | ',' | ':' | '*' | '_' | '~' | '\'' | '"' | ';') => {
                    end -= 1;
                }
                Some(')')
                    if text[start..end].matches(')').count()
                        > text[start..end].matches('(').count() =>
                {
                    end -= 1;
                }
                _ => break,
            }
        }
        let body = &text[start..end];
        let host = body.trim_start_matches(found.1);
        if boundary && host.contains('.') && !host.starts_with('.') && host.len() > 2 {
            let href = if found.1 == "www." {
                format!("http://{body}")
            } else {
                body.to_owned()
            };
            out.push((start..end, href));
            at = end;
        } else {
            at = start + found.1.len();
        }
    }
    out
}

#[cfg(test)]
mod tests;
