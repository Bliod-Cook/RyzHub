//! Code coloured by syntax: spans of text, each with a highlight.js class name the client
//! colours (`keyword`, `string`, `title`, …).
//!
//! The grammars are bat's (through two-face). Native builds carry them; the web loads them on
//! first use with [`load_syntaxes`] instead, which keeps them out of the wasm the page starts with.
use serde::Serialize;
use std::sync::OnceLock;
use syntect::parsing::{ParseState, Scope, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};

/// Longer code shows plain.
pub const MAX_HIGHLIGHTED: usize = 100_000;
/// Code blocks whose parse state is kept to continue as they grow.
const KEPT: usize = 32;

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

/// The grammars, once loaded.
fn syntaxes() -> Option<&'static SyntaxSet> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(SYNTAXES.get_or_init(two_face::syntax::extra_newlines))
    }
    #[cfg(target_arch = "wasm32")]
    {
        SYNTAXES.get()
    }
}

/// Loads the grammars from a dump of them (`cargo run -p ryzhub-core --example syntax-dump`
/// writes one). Loading again is a no-op.
pub fn load_syntaxes(dump: &[u8]) -> Result<(), String> {
    if SYNTAXES.get().is_some() {
        return Ok(());
    }
    let set: SyntaxSet = syntect::dumps::from_uncompressed_data(dump).map_err(|e| e.to_string())?;
    let _ = SYNTAXES.set(set);
    Ok(())
}

/// Whether code can be coloured now; on the web, not until [`load_syntaxes`].
pub fn ready() -> bool {
    syntaxes().is_some()
}

/// Fence labels and names models write that the grammars know by another name. Its `shell`
/// is a console session with prompts; a ```shell fence holds a plain script.
const ALIASES: &[(&str, &str)] = &[
    ("shell", "bash"),
    ("console", "bash"),
    ("zsh", "bash"),
    ("jsonl", "json"),
    ("jsonc", "json"),
    ("json5", "json"),
    ("htm", "html"),
    ("conf", "ini"),
    ("cfg", "ini"),
    ("node", "js"),
    ("javascript", "js"),
    ("typescript", "ts"),
    ("golang", "go"),
    ("objc", "m"),
    ("objective-c", "m"),
    ("postgres", "sql"),
    ("postgresql", "sql"),
    ("mysql", "sql"),
    ("sqlite", "sql"),
    ("protobuf", "proto"),
    ("patch", "diff"),
    ("docker", "dockerfile"),
    ("csharp", "cs"),
    ("c#", "cs"),
    ("kotlin", "kt"),
    ("tex", "latex"),
];

/// The grammar a fence's info string (its first word, e.g. `rust,ignore` or `json title=x`) or
/// a file extension names.
fn syntax(set: &'static SyntaxSet, name: &str) -> Option<&'static SyntaxReference> {
    let word = name
        .trim()
        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if word.is_empty() || matches!(word.as_str(), "text" | "txt" | "plain" | "plaintext") {
        return None;
    }
    let word = ALIASES
        .iter()
        .find(|(alias, _)| *alias == word)
        .map_or(word.as_str(), |(_, name)| name);
    set.find_syntax_by_token(word)
}

/// Whether `name` names a language code can be coloured in.
pub fn known(name: &str) -> bool {
    syntaxes().is_some_and(|set| syntax(set, name).is_some())
}

/// A run of code and its class; the empty class is plain text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Span(pub String, pub &'static str);

/// Scope prefixes and the class each gives, most specific first. A token takes the class of
/// its innermost scope that has one.
const RULES: &[(&str, &str)] = &[
    ("punctuation.definition.comment", "comment"),
    ("comment", "comment"),
    ("string.regexp", "regexp"),
    ("punctuation.definition.string", "string"),
    ("string", "string"),
    ("constant.numeric", "number"),
    ("constant.character.escape", "subst"),
    ("constant.language", "literal"),
    ("constant.other", "literal"),
    ("support.constant", "literal"),
    ("variable.language", "built_in"),
    ("variable.function", "title"),
    ("entity.name.function", "title"),
    ("support.function", "built_in"),
    ("entity.name.tag", "name"),
    ("entity.other.attribute-name", "attr"),
    ("support.type.property-name", "attr"),
    ("entity.name.section", "section"),
    ("markup.heading", "section"),
    ("entity.name.constant", "literal"),
    ("entity.name.namespace", ""),
    ("entity.name.label", "symbol"),
    ("entity.other.inherited-class", "class"),
    ("entity.name", "class"),
    ("support.type", "type"),
    ("support.class", "type"),
    ("storage.type", "keyword"),
    ("storage.modifier", "keyword"),
    ("keyword.operator.word", "keyword"),
    ("keyword.operator", ""),
    ("keyword", "keyword"),
    ("markup.bold", "strong"),
    ("markup.italic", "emphasis"),
    ("markup.inserted", "addition"),
    ("markup.deleted", "deletion"),
    ("markup.underline.link", "link"),
    ("markup.quote", "quote"),
    ("markup.list", "bullet"),
    ("markup.raw", "string"),
    ("meta.diff.header", "meta"),
    ("meta.diff.range", "meta"),
    ("meta.preprocessor", "meta"),
    ("meta.annotation", "meta"),
    ("variable.parameter", ""),
    ("punctuation", ""),
];

fn rules() -> &'static [(Scope, &'static str)] {
    static RULES_PARSED: OnceLock<Vec<(Scope, &'static str)>> = OnceLock::new();
    RULES_PARSED.get_or_init(|| {
        RULES
            .iter()
            .map(|(scope, class)| (Scope::new(scope).expect("valid scope"), *class))
            .collect()
    })
}

/// Keys of mappings (JSON, YAML, TOML) read as attributes, whatever their own scope.
fn mapping_key() -> Scope {
    static KEY: OnceLock<Scope> = OnceLock::new();
    *KEY.get_or_init(|| Scope::new("meta.mapping.key").unwrap())
}

fn class(stack: &ScopeStack) -> &'static str {
    let scopes = stack.as_slice();
    if scopes.iter().any(|s| mapping_key().is_prefix_of(*s)) {
        return "attr";
    }
    for scope in scopes.iter().rev() {
        if let Some((_, class)) = rules()
            .iter()
            .find(|(prefix, _)| prefix.is_prefix_of(*scope))
        {
            return class;
        }
    }
    ""
}

/// A code block's parse, kept to continue it: through its last complete line.
struct Kept {
    key: String,
    syntax: &'static SyntaxReference,
    /// The source through its last complete line.
    done: String,
    state: ParseState,
    stack: ScopeStack,
    spans: Vec<Span>,
}

/// Colours code, continuing where a growing code block left off.
#[derive(Default)]
pub struct Highlighter {
    /// Most recently used last.
    kept: Vec<Kept>,
}

impl Highlighter {
    /// `code` in `language`'s colours. `key` names the code block across calls, so a block that
    /// has grown is parsed only from its last complete line. None when the language is unknown,
    /// the grammars are not loaded yet or the code is too long.
    pub fn highlight(&mut self, key: &str, language: &str, code: &str) -> Option<Vec<Span>> {
        if code.len() > MAX_HIGHLIGHTED {
            return None;
        }
        let set = syntaxes()?;
        let syntax = syntax(set, language)?;
        let position = self.kept.iter().position(|k| k.key == key);
        let mut kept = match position.map(|i| self.kept.remove(i)) {
            Some(kept) if std::ptr::eq(kept.syntax, syntax) && code.starts_with(&kept.done) => kept,
            _ => Kept {
                key: key.to_owned(),
                syntax,
                done: String::new(),
                state: ParseState::new(syntax),
                stack: ScopeStack::new(),
                spans: vec![],
            },
        };
        let start = kept.done.len();
        let complete = code.rfind('\n').map_or(0, |n| n + 1).max(start);
        for line in code[start..complete].split_inclusive('\n') {
            parse(set, &mut kept.state, &mut kept.stack, line, &mut kept.spans)?;
        }
        kept.done.push_str(&code[start..complete]);
        let mut spans = kept.spans.clone();
        if complete < code.len() {
            // The line still being written is parsed again with the next call.
            let (mut state, mut stack) = (kept.state.clone(), kept.stack.clone());
            parse(set, &mut state, &mut stack, &code[complete..], &mut spans)?;
        }
        self.kept.push(kept);
        if self.kept.len() > KEPT {
            self.kept.remove(0);
        }
        Some(spans)
    }
}

/// Parses one line, adding its spans; adjacent spans of a class join.
fn parse(
    set: &SyntaxSet,
    state: &mut ParseState,
    stack: &mut ScopeStack,
    line: &str,
    spans: &mut Vec<Span>,
) -> Option<()> {
    // A grammar failing on whatever a model wrote leaves the code plain.
    let ops = state.parse_line(line, set).ok()?;
    let mut cursor = 0;
    let push = |spans: &mut Vec<Span>, text: &str, class: &'static str| {
        if text.is_empty() {
            return;
        }
        match spans.last_mut() {
            Some(Span(last, c)) if *c == class => last.push_str(text),
            _ => spans.push(Span(text.to_owned(), class)),
        }
    };
    for (at, op) in ops {
        if at > cursor {
            push(spans, &line[cursor..at], class(stack));
            cursor = at;
        }
        apply(stack, &op)?;
    }
    push(spans, &line[cursor..], class(stack));
    Some(())
}

fn apply(stack: &mut ScopeStack, op: &ScopeStackOp) -> Option<()> {
    stack.apply(op).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(spans: &[Span], text: &str) -> Vec<&'static str> {
        spans
            .iter()
            .filter(|s| s.0.contains(text))
            .map(|s| s.1)
            .collect()
    }

    #[test]
    fn languages_colour_their_tokens() {
        let mut h = Highlighter::default();
        let rust = h
            .highlight(
                "a",
                "rust",
                "fn main() {\n    let s = \"hi\"; // note\n    1\n}\n",
            )
            .unwrap();
        assert_eq!(
            rust.iter().map(|s| s.0.as_str()).collect::<String>().len(),
            46
        );
        assert_eq!(classes(&rust, "fn"), ["keyword"]);
        assert_eq!(classes(&rust, "main"), ["title"]);
        assert!(classes(&rust, "\"hi").contains(&"string"));
        assert!(classes(&rust, "// note").contains(&"comment"));
        for (language, code) in [
            ("ts", "const x: number = 1;\n"),
            ("typescript", "let a = 1\n"),
            ("tsx", "const a = <div/>\n"),
            ("py", "def f(): pass\n"),
            ("shell", "echo hi\n"),
            ("json", "{\"a\": 1}\n"),
            ("toml", "a = 1\n"),
            ("dockerfile", "FROM x\n"),
            ("kotlin", "fun main() {}\n"),
            ("swift", "func f() {}\n"),
            ("dart", "void main() {}\n"),
            ("yaml", "a: 1\n"),
            ("diff", "-a\n+b\n"),
            ("sql", "SELECT 1\n"),
            ("go", "package main\n"),
        ] {
            assert!(
                h.highlight(language, language, code).is_some(),
                "{language}"
            );
        }
        assert!(h.highlight("x", "no-such-language", "x").is_none());
        let json = h.highlight("j", "json", "{\"key\": \"value\"}").unwrap();
        assert_eq!(classes(&json, "key"), ["attr"]);
        assert_eq!(classes(&json, "value"), ["string"]);
    }

    #[test]
    fn growing_code_continues_from_its_last_line() {
        let mut h = Highlighter::default();
        let whole = "/* a\nb */ let x = 1;\nlet y";
        let mut grown = None;
        for end in 1..=whole.len() {
            grown = h.highlight("k", "js", &whole[..end]);
        }
        let fresh = Highlighter::default().highlight("other", "js", whole);
        assert_eq!(grown, fresh);
        assert!(classes(grown.as_ref().unwrap(), "b */").contains(&"comment"));
    }
}
