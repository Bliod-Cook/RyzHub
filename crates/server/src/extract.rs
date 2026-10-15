//! Text from documents. A PDF or DOCX file becomes the text models read; the parsing runs in
//! a process of its own (`ryzhub-server extract-text <format>`) under memory and CPU limits,
//! so a hostile file cannot take the server with it.
use std::{
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, ensure};
use serde_json::json;
use tokio::sync::Semaphore;

use crate::error::Code;

/// The largest document read.
pub const MAX_SOURCE: u64 = 32 * 1024 * 1024;
/// The most text kept from one document.
pub const MAX_TEXT: usize = 1024 * 1024;
const CHILD_MEMORY: u64 = 1024 * 1024 * 1024;
const CHILD_CPU_SECS: u64 = 30;
const TIMEOUT: Duration = Duration::from_secs(45);
/// A DOCX's document part may expand to at most this much.
const MAX_PART: u64 = 64 * 1024 * 1024;
static EXTRACTIONS: Semaphore = Semaphore::const_new(2);

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Docx,
    Text,
}

impl Format {
    /// What `head` (the file's first bytes) and `name` say the file is, and its media type.
    /// Anything else is not a document this reads.
    pub fn of(head: &[u8], name: &str) -> Option<(Self, &'static str)> {
        let lower = name.to_ascii_lowercase();
        if head.starts_with(b"%PDF-") {
            Some((Self::Pdf, "application/pdf"))
        } else if head.starts_with(b"PK\x03\x04") {
            lower.ends_with(".docx").then_some((Self::Docx, DOCX))
        } else if !head.contains(&0) && utf8_prefix(head) {
            let media = if lower.ends_with(".md") || lower.ends_with(".markdown") {
                "text/markdown"
            } else {
                "text/plain"
            };
            Some((Self::Text, media))
        } else {
            None
        }
    }

    fn arg(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Text => "text",
        }
    }
}

/// `head` is UTF-8, allowing a character cut off at its end.
fn utf8_prefix(head: &[u8]) -> bool {
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none() && head.len() - error.valid_up_to() < 4,
    }
}

/// The text of a document.
#[derive(Debug, PartialEq, Eq)]
pub struct Extracted {
    pub text: String,
    /// The text was longer than [`MAX_TEXT`] and was cut there.
    pub truncated: bool,
}

/// The text of the `format` document at `path`.
pub async fn extract(program: &Path, format: Format, path: &Path) -> anyhow::Result<Extracted> {
    if format == Format::Text {
        let bytes = tokio::fs::read(path).await?;
        let text = String::from_utf8(bytes).map_err(|_| Code::UnsupportedProjectFile)?;
        return Ok(bounded(text));
    }
    let _permit = EXTRACTIONS.acquire().await?;
    let input = tokio::fs::File::open(path).await?;
    let args = ["extract-text", format.arg()];
    let reply = crate::child::run(program, &args, input, 4 * MAX_TEXT as u64, TIMEOUT)
        .await
        .context(Code::DocumentUnreadable)?;
    if let Some(error) = reply["error"].as_str() {
        tracing::info!(error, "document not read");
        return Err(anyhow::Error::new(crate::error::Detail(error.to_owned()))
            .context(Code::DocumentUnreadable));
    }
    Ok(Extracted {
        text: reply["text"].as_str().unwrap_or_default().to_owned(),
        truncated: reply["truncated"].as_bool().unwrap_or(false),
    })
}

/// `ryzhub-server extract-text <pdf|docx>`: read a document on stdin and write its text as
/// JSON on stdout.
pub fn child_main(args: &[String]) -> anyhow::Result<()> {
    use nix::sys::resource::{Resource, setrlimit};
    setrlimit(Resource::RLIMIT_AS, CHILD_MEMORY, CHILD_MEMORY)?;
    setrlimit(Resource::RLIMIT_CPU, CHILD_CPU_SECS, CHILD_CPU_SECS)?;
    let format = args.first().context("format")?.clone();
    let mut bytes = vec![];
    std::io::stdin()
        .lock()
        .take(MAX_SOURCE + 1)
        .read_to_end(&mut bytes)?;
    let reply = match (|| {
        ensure!(
            bytes.len() as u64 <= MAX_SOURCE,
            "documents over {MAX_SOURCE} bytes are not read"
        );
        let text = match format.as_str() {
            "pdf" => pdf_extract::extract_text_from_mem(&bytes).context("reading the PDF")?,
            "docx" => docx_text(&bytes)?,
            other => anyhow::bail!("unknown format {other}"),
        };
        let extracted = bounded(tidy(&text));
        Ok(json!({"text": extracted.text, "truncated": extracted.truncated}))
    })() {
        Ok(value) => value,
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &reply)?;
    out.flush()?;
    Ok(())
}

/// The paragraphs of a DOCX's main document, one per line.
fn docx_text(bytes: &[u8]) -> anyhow::Result<String> {
    use quick_xml::{Reader, events::Event};
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("not a DOCX package")?;
    let mut xml = vec![];
    archive
        .by_name("word/document.xml")
        .context("no word/document.xml")?
        .take(MAX_PART + 1)
        .read_to_end(&mut xml)?;
    ensure!(xml.len() as u64 <= MAX_PART, "document part too large");
    let mut reader = Reader::from_reader(xml.as_slice());
    let (mut text, mut buffer, mut inside) = (String::new(), vec![], false);
    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(tag) if tag.local_name().as_ref() == b"t" => inside = true,
            Event::End(tag) => match tag.local_name().as_ref() {
                b"t" => inside = false,
                b"p" => text.push('\n'),
                _ => {}
            },
            Event::Empty(tag) => match tag.local_name().as_ref() {
                b"tab" => text.push('\t'),
                b"br" | b"cr" => text.push('\n'),
                _ => {}
            },
            Event::Text(content) if inside => text.push_str(&content.decode()?),
            Event::GeneralRef(reference) if inside => {
                if let Some(c) = reference.resolve_char_ref()? {
                    text.push(c);
                } else if let Some(s) =
                    quick_xml::escape::resolve_predefined_entity(&reference.decode()?)
                {
                    text.push_str(s);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
        ensure!(text.len() <= 4 * MAX_TEXT, "document text too large");
    }
    Ok(text)
}

/// Trailing spaces off every line, and at most one blank line in a row.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

fn bounded(mut text: String) -> Extracted {
    if text.len() <= MAX_TEXT {
        return Extracted {
            text,
            truncated: false,
        };
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    Extracted {
        text,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docx(document: &str) -> Vec<u8> {
        let mut out = std::io::Cursor::new(vec![]);
        let mut zip = zip::ZipWriter::new(&mut out);
        zip.start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn docx_paragraphs_become_lines() {
        let xml = r#"<?xml version="1.0"?><w:document xmlns:w="w"><w:body>
            <w:p><w:r><w:t>Quarterly</w:t></w:r><w:r><w:t xml:space="preserve"> report &amp; plan</w:t></w:r></w:p>
            <w:p><w:r><w:t>季度</w:t><w:tab/><w:t>&#x41;</w:t><w:br/><w:t>next</w:t></w:r></w:p>
            <w:p><w:r><w:instrText>IGNORED</w:instrText></w:r></w:p>
        </w:body></w:document>"#;
        let text = tidy(&docx_text(&docx(xml)).unwrap());
        assert_eq!(text, "Quarterly report & plan\n季度\tA\nnext");
        assert!(docx_text(b"PK\x03\x04 not really").is_err());
    }

    /// A one-page PDF showing `text` in a standard font.
    pub(crate) fn pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_owned(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![];
        for (n, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", n + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    #[test]
    fn pdf_text_is_read() {
        let text = pdf_extract::extract_text_from_mem(&pdf("Quarterly plan")).unwrap();
        assert_eq!(tidy(&text), "Quarterly plan");
    }

    #[test]
    fn formats_come_from_content_and_text_is_bounded() {
        assert_eq!(Format::of(b"%PDF-1.7\n", "a.bin").unwrap().0, Format::Pdf);
        assert_eq!(
            Format::of(b"PK\x03\x04", "Plan.DOCX").unwrap().0,
            Format::Docx
        );
        assert!(Format::of(b"PK\x03\x04", "plan.zip").is_none());
        assert_eq!(
            Format::of("笔记".as_bytes(), "n.md").unwrap(),
            (Format::Text, "text/markdown")
        );
        // A character cut off by the sniffing window is still text.
        assert!(Format::of(&"笔".as_bytes()[..2], "n.txt").is_some());
        assert!(Format::of(b"\x00\x01", "n.txt").is_none());
        let long = bounded("é".repeat(MAX_TEXT));
        assert!(long.truncated && long.text.len() <= MAX_TEXT);
        assert_eq!(tidy("a  \n\n\n\nb\n"), "a\n\nb");
    }
}
