use super::*;

fn text(s: &str) -> Inline {
    Inline::Text {
        text: s.into(),
        style: 0,
        href: None,
    }
}

fn styled(s: &str, style: u8) -> Inline {
    Inline::Text {
        text: s.into(),
        style,
        href: None,
    }
}

fn blocks(source: &str) -> Vec<Block> {
    render(source, &[], false).blocks
}

fn inlines(source: &str) -> Vec<Inline> {
    match blocks(source).as_slice() {
        [Block::Paragraph { inlines }] => inlines.clone(),
        other => panic!("not one paragraph: {other:?}"),
    }
}

#[test]
fn blocks_and_styles() {
    assert_eq!(
        inlines("a **b** *c* ~~d~~ `e`"),
        vec![
            text("a "),
            styled("b", STRONG),
            text(" "),
            styled("c", EMPHASIS),
            text(" "),
            styled("d", STRIKE),
            text(" "),
            Inline::Code {
                text: "e".into(),
                href: None
            },
        ]
    );
    let doc = blocks("# Title\n\n- a\n- [x] b\n\n> quote\n\n---\n\n```rust\nfn main() {}\n```\n");
    assert!(matches!(&doc[0], Block::Heading { level: 1, .. }));
    let Block::List { tight, items, .. } = &doc[1] else {
        panic!("{doc:?}")
    };
    assert!(tight);
    assert_eq!(items[1].task, Some(true));
    assert!(matches!(&doc[2], Block::Quote { .. }));
    assert_eq!(doc[3], Block::Rule);
    assert_eq!(
        doc[4],
        Block::Code {
            info: "rust".into(),
            text: "fn main() {}\n".into()
        }
    );
    let Block::List { tight, .. } = &blocks("- a\n\n- b\n")[0] else {
        panic!()
    };
    assert!(!tight, "blank lines between items make a loose list");
    let Block::Table { align, head, rows } = &blocks("| a | b |\n|:--|--:|\n| 1 | 2 |\n")[0] else {
        panic!()
    };
    assert_eq!(align, &vec!["left", "right"]);
    assert_eq!(head[0], vec![text("a")]);
    assert_eq!(rows[0][1], vec![text("2")]);
}

#[test]
fn every_delimiter_reads_tex() {
    for (source, tex, display) in [
        (r"$$x = \frac{1}{2}$$", r"x = \frac{1}{2}", true),
        (r"\[E = mc^2\]", "E = mc^2", true),
        (r"\(\alpha\)", r"\alpha", false),
        (r"$a^2+b^2=c^2$", "a^2+b^2=c^2", false),
    ] {
        let found = render(source, &[], false).blocks;
        let math = match &found[..] {
            [Block::Math { tex, .. }] => (tex.clone(), true),
            [Block::Paragraph { inlines }] => match &inlines[..] {
                [Inline::Math { tex, display, .. }] => (tex.clone(), *display),
                other => panic!("{source}: {other:?}"),
            },
            other => panic!("{source}: {other:?}"),
        };
        assert_eq!(math, (tex.to_owned(), display), "{source}");
    }
    // A displayed formula within text stays in its paragraph; the text around it remains.
    let found = inlines(r"前文 \[E = mc^2\] 后文");
    assert!(matches!(&found[1], Inline::Math { display: true, .. }));
    assert_eq!(found[0], text("前文 "));
    // Lines of a formula that read as Markdown stay in it, across a blank line.
    let doc = blocks("推导：\n$$\nx = a\n- b\n\n+ c\n$$\n结束");
    assert_eq!(doc.len(), 1, "{doc:?}");
    // Underscores are subscripts, not emphasis.
    assert!(matches!(&inlines("$a_1 + b_2$")[0], Inline::Math { tex, .. } if tex == "a_1 + b_2"));
}

#[test]
fn prices_code_and_escapes_stay_text() {
    for source in ["costs $5 and $10", r"\$x\$", "`$x$`", "a $ b $ c", "$x$1"] {
        let found = render(source, &[], false).blocks;
        let json = serde_json::to_string(&found).unwrap();
        assert!(!json.contains("\"math\""), "{source}: {json}");
    }
    assert!(
        blocks("```\n$x$\n```")
            .iter()
            .all(|b| matches!(b, Block::Code { .. }))
    );
}

#[test]
fn sandbox_files_and_bare_addresses_link() {
    assert_eq!(
        inlines("see [the chart](sandbox:/workspace/out/chart%201.png)"),
        vec![
            text("see "),
            Inline::File {
                label: "the chart".into(),
                href: "sandbox:/workspace/out/chart%201.png".into()
            }
        ]
    );
    assert!(matches!(
        &inlines("[a](/attachments/x.pdf)")[0],
        Inline::File { href, .. } if href == "/attachments/x.pdf"
    ));
    assert_eq!(
        inlines("go to https://example.com/a_(b). Or www.rust-lang.org!"),
        vec![
            text("go to "),
            Inline::Text {
                text: "https://example.com/a_(b)".into(),
                style: 0,
                href: Some("https://example.com/a_(b)".into())
            },
            text(". Or "),
            Inline::Text {
                text: "www.rust-lang.org".into(),
                style: 0,
                href: Some("http://www.rust-lang.org".into())
            },
            text("!"),
        ]
    );
    // An image shows as a link to it.
    assert_eq!(
        inlines("![a cat](https://x.org/cat.png)"),
        vec![Inline::Image {
            alt: "a cat".into(),
            url: "https://x.org/cat.png".into()
        }]
    );
}

#[test]
fn cited_sources_fold_into_chips() {
    let cited = vec!["https://a.com/x".to_owned(), "https://b.org".to_owned()];
    let source = "Fact ([A](https://a.com/x)) ([B](https://b.org), [A again](https://a.com/x)). Other ([C](https://c.net)).";
    let out = render(source, &cited, false);
    let Block::Paragraph { inlines } = &out.blocks[0] else {
        panic!()
    };
    assert_eq!(inlines[0], text("Fact "));
    assert_eq!(inlines[1], Inline::Chip { index: 0 });
    assert_eq!(
        out.chips,
        vec![vec![
            Source {
                label: "A".into(),
                url: "https://a.com/x".into()
            },
            Source {
                label: "B".into(),
                url: "https://b.org".into()
            },
        ]]
    );
    // A page not cited stays a link.
    let json = serde_json::to_string(&out.blocks).unwrap();
    assert!(json.contains("https://c.net"), "{json}");
    // While streaming, a run not finished, or not yet cited, waits hidden.
    for partial in [
        "Fact ([A](https://a.com",
        "Fact ([A](https://a.com/x), [",
        "Fact ([C](https://c.net)) ",
    ] {
        let out = render(partial, &cited, true);
        assert_eq!(out.blocks, blocks("Fact"), "{partial}");
    }
}

#[test]
fn streaming_closes_what_its_end_leaves_open() {
    let streamed = |s: &str| match &render(s, &[], true).blocks[..] {
        [Block::Paragraph { inlines }] => inlines.clone(),
        other => panic!("{s}: {other:?}"),
    };
    assert_eq!(
        streamed("a **bold"),
        vec![text("a "), styled("bold", STRONG)]
    );
    assert_eq!(
        streamed("a `co"),
        vec![
            text("a "),
            Inline::Code {
                text: "co".into(),
                href: None
            }
        ]
    );
    assert_eq!(
        streamed("see [the docs](https://exa"),
        vec![text("see the docs")]
    );
    assert_eq!(
        streamed("a ~~gone"),
        vec![text("a "), styled("gone", STRIKE)]
    );
    // A marker with nothing after it yet is dropped rather than closed.
    assert_eq!(streamed("a **"), vec![text("a")]);
    // A product is not emphasis.
    assert_eq!(streamed("2 * 3"), vec![text("2 * 3")]);
    // Finished, the text renders as written.
    assert_eq!(inlines("a **bold"), vec![text("a **bold")]);
}

/// Deterministic pseudo-random numbers.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const LINES: &[&str] = &[
    "# Title",
    "Heading",
    "===",
    "---",
    "para text with **bold** and *em*",
    "more of it, `code` and $x^2$",
    "",
    "",
    "- item",
    "- [ ] task",
    "  continued",
    "1. first",
    "2) second",
    "```rust",
    "fn main() { let x = \"$\"; }",
    "```",
    "~~~",
    "> quote",
    "> > nested",
    "$$",
    "x = a",
    "\\[ y \\]",
    "| a | b |",
    "|---|:-:|",
    "| 1 | 2 |",
    "    indented code",
    "<div>",
    "</div>",
    "Fact ([A](https://a.com/x)) and ([B](https://b.org)",
    "see [docs](https://exa",
    "[label](sandbox:/workspace/a.txt) www.example.com",
    "#",
    "#hash",
    "***",
    "text with **open",
    "中文段落，含 \\(\\alpha\\) 公式",
];

fn corpus(rng: &mut Rng) -> String {
    let lines = 3 + rng.below(25);
    (0..lines)
        .map(|_| LINES[rng.below(LINES.len())])
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_streams(source: &str, cited: &[String], rng: &mut Rng) {
    let mut doc = Doc::default();
    let mut shown: Vec<Block> = vec![];
    let mut at = 0;
    while at < source.len() {
        let mut next = (at + 1 + rng.below(12)).min(source.len());
        while !source.is_char_boundary(next) {
            next += 1;
        }
        at = next;
        let prefix = &source[..at];
        let splice = doc.update(prefix, cited, true);
        let full = render(prefix, cited, true);
        let incremental = doc.rendered();
        assert_eq!(incremental, full, "streamed {prefix:?}");
        // What the splice says is new brings an earlier view up to date.
        shown.truncate(splice.from);
        shown.extend(
            incremental.blocks[splice.from.min(incremental.blocks.len())..]
                .iter()
                .cloned(),
        );
        assert_eq!(shown, incremental.blocks, "spliced {prefix:?}");
    }
    doc.update(source, cited, false);
    assert_eq!(
        doc.rendered(),
        render(source, cited, false),
        "finished {source:?}"
    );
}

#[test]
fn streaming_in_any_chunks_renders_as_a_whole_parse() {
    let cited = vec!["https://a.com/x".to_owned()];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..400 {
        let source = corpus(&mut rng);
        assert_streams(&source, &cited, &mut rng);
    }
    // Definitions anywhere change blocks anywhere: such a message is never frozen.
    let source = "[a][r] and [^1]\n\npara\n\n[r]: https://x.org\n[^1]: note\n";
    assert_streams(source, &cited, &mut rng);
}

#[test]
fn finished_blocks_freeze() {
    let mut doc = Doc::default();
    doc.update("first paragraph\n\nsecond line\nthird", &[], true);
    let splice = doc.update("first paragraph\n\nsecond line\nthird grows", &[], true);
    assert_eq!(splice.from, 1, "only the last block is new");
    assert_eq!(doc.frozen.blocks.len(), 1);
    // A replaced message renders afresh.
    let splice = doc.update("other", &[], false);
    assert_eq!(splice.from, 0);
}
