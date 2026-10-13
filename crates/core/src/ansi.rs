//! Terminal output as styled spans: SGR colours and styles kept, other control sequences
//! dropped. A carriage return lets the line after it overwrite the line before, as progress
//! bars expect.
use serde::Serialize;
use vte::{Params, Parser, Perform};

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const ITALIC: u8 = 4;
pub const UNDERLINE: u8 = 8;
pub const INVERSE: u8 = 16;
pub const STRIKE: u8 = 32;

/// A colour: one of the 256 of the palette, or `#rrggbb`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Color {
    Palette(u8),
    Rgb(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AnsiSpan {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<Color>,
    #[serde(skip_serializing_if = "is_zero")]
    pub style: u8,
}

fn is_zero(style: &u8) -> bool {
    *style == 0
}

/// `text` as it shows in a terminal that keeps no cursor position beyond its line.
pub fn parse(text: &str) -> Vec<AnsiSpan> {
    let mut screen = Screen::default();
    let mut parser = Parser::new();
    parser.advance(&mut screen, text.as_bytes());
    screen.spans.retain(|s| !s.text.is_empty());
    screen.spans
}

#[derive(Default)]
struct Screen {
    spans: Vec<AnsiSpan>,
    pen: AnsiSpan,
    /// Where the current line starts: a span and the byte in it.
    line: (usize, usize),
    /// A carriage return came: what is printed next replaces the line.
    returned: bool,
}

impl Screen {
    fn put(&mut self, c: char) {
        if self.returned {
            self.returned = false;
            let (span, at) = self.line;
            self.spans.truncate(span + 1);
            if let Some(s) = self.spans.get_mut(span) {
                s.text.truncate(at);
            }
        }
        let same = self.spans.last().is_some_and(|s| {
            s.fg == self.pen.fg && s.bg == self.pen.bg && s.style == self.pen.style
        });
        if !same {
            self.spans.push(AnsiSpan {
                text: String::new(),
                ..self.pen.clone()
            });
        }
        self.spans.last_mut().unwrap().text.push(c);
        if c == '\n' {
            let last = self.spans.len() - 1;
            self.line = (last, self.spans[last].text.len());
        }
    }

    fn sgr(&mut self, params: &Params) {
        let mut values = params.iter().flat_map(|p| p.iter().copied());
        // An empty SGR resets.
        let mut any = false;
        while let Some(code) = values.next() {
            any = true;
            let pen = &mut self.pen;
            match code {
                0 => *pen = AnsiSpan::default(),
                1 => pen.style |= BOLD,
                2 => pen.style |= DIM,
                3 => pen.style |= ITALIC,
                4 => pen.style |= UNDERLINE,
                7 => pen.style |= INVERSE,
                9 => pen.style |= STRIKE,
                22 => pen.style &= !(BOLD | DIM),
                23 => pen.style &= !ITALIC,
                24 => pen.style &= !UNDERLINE,
                27 => pen.style &= !INVERSE,
                29 => pen.style &= !STRIKE,
                30..=37 => pen.fg = Some(Color::Palette((code - 30) as u8)),
                90..=97 => pen.fg = Some(Color::Palette((code - 90 + 8) as u8)),
                40..=47 => pen.bg = Some(Color::Palette((code - 40) as u8)),
                100..=107 => pen.bg = Some(Color::Palette((code - 100 + 8) as u8)),
                39 => pen.fg = None,
                49 => pen.bg = None,
                38 | 48 => {
                    let color = match values.next() {
                        Some(5) => values.next().map(|i| Color::Palette(i.min(255) as u8)),
                        Some(2) => {
                            let (r, g, b) = (values.next(), values.next(), values.next());
                            r.zip(g).zip(b).map(|((r, g), b)| {
                                Color::Rgb(format!(
                                    "#{:02x}{:02x}{:02x}",
                                    r.min(255),
                                    g.min(255),
                                    b.min(255)
                                ))
                            })
                        }
                        _ => None,
                    };
                    if code == 38 {
                        pen.fg = color;
                    } else {
                        pen.bg = color;
                    }
                }
                _ => {}
            }
        }
        if !any {
            self.pen = AnsiSpan::default();
        }
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.returned = false;
                self.put('\n');
            }
            b'\t' => self.put('\t'),
            b'\r' => self.returned = true,
            // Backspace takes back the last character of the line.
            0x08 => {
                let (span, at) = self.line;
                let count = self.spans.len();
                if let Some(last) = self.spans.last_mut()
                    && (count - 1 > span || last.text.len() > at)
                {
                    last.text.pop();
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _: &[u8], _: bool, action: char) {
        match action {
            'm' => self.sgr(params),
            // Erasing the line after a carriage return is what the return does already.
            'K' if self.returned => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_styles_and_overwritten_lines() {
        let spans = parse("\x1b[1;31merror\x1b[0m: plain \x1b[38;5;208mo\x1b[38;2;1;2;3mr\x1b[m\n");
        assert_eq!(
            spans[0],
            AnsiSpan {
                text: "error".into(),
                fg: Some(Color::Palette(1)),
                bg: None,
                style: BOLD
            }
        );
        assert_eq!(spans[1].text, ": plain ");
        assert_eq!(spans[2].fg, Some(Color::Palette(208)));
        assert_eq!(spans[3].fg, Some(Color::Rgb("#010203".into())));
        let progress = parse("start\n10%\r\x1b[K50%\r100%\ndone");
        let shown: String = progress.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(shown, "start\n100%\ndone");
        // Other control sequences leave no trace.
        assert_eq!(parse("a\x1b[2Jb\x1b]0;title\x07c")[0].text, "abc");
    }
}
