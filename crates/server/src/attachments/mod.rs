//! Each member's file library. A file belongs to its uploader; any of the uploader's
//! conversations may refer to it, and models see it there both as content and as a read-only
//! file under `/attachments`. Deleting a file removes it everywhere; deleting a conversation
//! removes only its references.
pub mod gc;
pub mod http;
pub(crate) mod ingest;
pub mod outputs;
pub mod sandbox;
pub mod uploads;

use std::{path::Path, time::Duration};

use tokio::sync::Semaphore;
use uuid::Uuid;

use anyhow::Context;
use ryzhub_proto::Attachment;

use crate::{
    app::App,
    chat::media as images,
    error::Code,
    store::{Lease, Tx},
};

/// Largest file anyone may upload; policy may only lower it.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Upload limit when policy sets none.
const DEFAULT_BYTES: u64 = 50 * 1024 * 1024;
/// Files one message may carry.
pub const MAX_PER_MESSAGE: usize = 10;
/// UTF-8 files up to this size are inlined into model context.
pub const TEXT_INLINE: usize = 32 * 1024;
/// Display names, in characters.
const MAX_NAME: usize = 255;
/// File names under `/attachments`, in bytes.
const MAX_PATH_NAME: usize = 128;
/// Images decoded at once, and how long one may take.
static RENDERS: Semaphore = Semaphore::const_new(2);
const RENDER_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Pdf,
    Text,
    File,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Pdf => "pdf",
            Self::Text => "text",
            Self::File => "file",
        }
    }

    pub fn parse(kind: &str) -> Option<Self> {
        [Self::Image, Self::Pdf, Self::Text, Self::File]
            .into_iter()
            .find(|known| known.as_str() == kind)
    }
}

/// What a file is, from its first bytes (`head` holds the whole file when it is small enough to
/// inline) and its name. The client's claimed type is never consulted.
pub fn sniff(head: &[u8], size: u64, name: &str) -> (Kind, &'static str) {
    let image = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else if head.starts_with(b"BM") && head.len() >= 26 {
        Some("image/bmp")
    } else {
        None
    };
    if let Some(media_type) = image {
        return (Kind::Image, media_type);
    }
    if head.starts_with(b"%PDF-") {
        return (Kind::Pdf, "application/pdf");
    }
    let named = named_type(name);
    if size as usize <= TEXT_INLINE
        && head.len() as u64 == size
        && !head.contains(&0)
        && std::str::from_utf8(head).is_ok()
    {
        return (
            Kind::Text,
            named
                .filter(|t| t.starts_with("text/"))
                .unwrap_or("text/plain"),
        );
    }
    (Kind::File, named.unwrap_or("application/octet-stream"))
}

/// A descriptive type for common extensions; shown to models, never trusted for serving.
fn named_type(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "xml" => "text/xml",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        _ => return None,
    })
}

/// The name shown for an upload: trimmed, without control characters, bounded.
pub fn display_name(raw: &str) -> Option<String> {
    let name: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect::<String>()
        .trim()
        .to_owned();
    (!name.is_empty()).then_some(name)
}

/// A file name for `/attachments`: no separators or leading dot, bounded in bytes with the
/// extension kept, and distinct from every name `taken` reports.
pub fn path_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = match clean.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None => clean,
    };
    let (stem, extension) = match clean.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 16 => {
            (stem.to_owned(), format!(".{extension}"))
        }
        _ => (clean.clone(), String::new()),
    };
    let fit = |stem: &str, suffix: &str| {
        let room = MAX_PATH_NAME.saturating_sub(suffix.len() + extension.len());
        let mut end = stem.len().min(room);
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{suffix}{extension}", &stem[..end])
    };
    let mut candidate = fit(&stem, "");
    let mut n = 2;
    while taken(&candidate) {
        candidate = fit(&stem, &format!(" ({n})"));
        n += 1;
    }
    candidate
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// What a Work turn says about the files it carries: a list naming each under
/// `/attachments`, and the images the model can take as they are.
pub(crate) async fn turn_files(
    tx: &mut Tx<'_>,
    lease: &Lease,
    ids: &[Uuid],
) -> anyhow::Result<(String, Vec<String>)> {
    let rows: Vec<(Uuid, String, String, String, i64, bool)> = sqlx::query_as(
        "SELECT a.id,f.path_name,a.media_type,a.kind,a.size,coalesce(a.model_sha=a.sha256,false)
         FROM attachments a JOIN conversation_files f
           ON f.org_id=a.org_id AND f.attachment_id=a.id AND f.conversation_id=$2
         WHERE a.org_id=$1 AND a.id=ANY($3) AND a.state='ready'",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut lines = vec![];
    let mut images = vec![];
    for id in ids {
        let Some((_, name, media_type, kind, size, intact)) = rows.iter().find(|r| r.0 == *id)
        else {
            lines.push("- (a file that was deleted)".to_owned());
            continue;
        };
        let path = format!("/attachments/{name}");
        let mut line = format!("- {path} ({media_type}, {})", human_size(*size as u64));
        if kind == "image" && *intact {
            images.push(path);
        } else if kind == "image" {
            line.push_str(", open it with your image tool");
        }
        lines.push(line);
    }
    Ok((
        format!("Attached files (read-only):\n{}", lines.join("\n")),
        images,
    ))
}

/// Refer to `user`'s files `ids` from the lease's conversation, naming each under
/// `/attachments` the first time. Runs inside the command's savepoint, so a rejected command
/// refers to nothing.
pub(crate) async fn bind(
    tx: &mut Tx<'_>,
    lease: &Lease,
    user: Uuid,
    ids: &[String],
) -> anyhow::Result<Vec<(Uuid, Attachment)>> {
    let mut files = Vec::with_capacity(ids.len());
    for raw in ids {
        let id = Uuid::parse_str(raw)
            .ok()
            .context(Code::AttachmentUnavailable)?;
        // Shared: a concurrent delete waits for this command, then removes the reference too.
        type Row = (String, String, String, i64, Option<i32>, Option<i32>);
        let row: Option<Row> = sqlx::query_as(
            "SELECT name,media_type,kind,size,width,height FROM attachments
             WHERE org_id=$1 AND id=$2 AND user_id=$3 AND state='ready' FOR SHARE",
        )
        .bind(lease.org_id)
        .bind(id)
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?;
        let (name, media_type, kind, size, width, height) =
            row.context(Code::AttachmentUnavailable)?;
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT path_name FROM conversation_files WHERE org_id=$1 AND conversation_id=$2 AND attachment_id=$3",
        )
        .bind(lease.org_id)
        .bind(lease.conversation_id)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
        let path = match existing {
            Some(path) => path,
            None => {
                let taken: std::collections::HashSet<String> = sqlx::query_scalar(
                    "SELECT path_name FROM conversation_files WHERE org_id=$1 AND conversation_id=$2",
                )
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .fetch_all(&mut **tx)
                .await?
                .into_iter()
                .collect();
                let path = path_name(&name, |candidate| taken.contains(candidate));
                sqlx::query(
                    "INSERT INTO conversation_files(org_id,conversation_id,attachment_id,path_name)
                     VALUES($1,$2,$3,$4)",
                )
                .bind(lease.org_id)
                .bind(lease.conversation_id)
                .bind(id)
                .bind(&path)
                .execute(&mut **tx)
                .await?;
                path
            }
        };
        let side = |side: Option<i32>| side.and_then(|side| u32::try_from(side).ok()).unwrap_or(0);
        files.push((
            id,
            Attachment {
                id: id.to_string(),
                path,
                name,
                media_type,
                size: size as u64,
                kind,
                width: side(width),
                height: side(height),
            },
        ));
    }
    Ok(files)
}

/// Serialize storing and reclaiming one content address of `org`'s library.
pub(crate) async fn content_lock(tx: &mut Tx<'_>, org: Uuid, sha256: &str) -> anyhow::Result<()> {
    crate::store::advisory_lock(tx, &format!("attachment:{org}:{sha256}")).await
}

/// The largest file `policy` lets a member add to their library.
pub fn limit(policy: &ryzhub_domain::Policy) -> u64 {
    policy
        .attachment_bytes
        .unwrap_or(DEFAULT_BYTES)
        .min(MAX_BYTES)
}

/// An uploaded image as models see it.
pub(crate) struct Rendition {
    pub media_type: &'static str,
    pub data: Vec<u8>,
    /// The original's size, upright.
    pub original: (u32, u32),
}

/// Decode the image in `file` with a `render-image` process. `None` when it is not an image
/// the decoder accepts, or decoding fails or takes too long; the reason is logged.
pub(crate) async fn render(app: &App, file: &Path) -> Option<Rendition> {
    match rendition(app, file).await {
        Ok(rendition) => Some(rendition),
        Err(error) => {
            tracing::warn!(error = format!("{error:#}"), "image not rendered");
            None
        }
    }
}

async fn rendition(app: &App, file: &Path) -> anyhow::Result<Rendition> {
    let _permit = RENDERS.acquire().await?;
    let program = crate::child::program(&app.config.attachment_renderer)?;
    let input = tokio::fs::File::open(file).await?;
    let (edge, bytes) = (images::MAX_EDGE.to_string(), images::MAX_BYTES.to_string());
    let reply = crate::child::run(
        &program,
        &["render-image", &edge, &bytes],
        input,
        2 * images::MAX_BYTES as u64,
        RENDER_TIMEOUT,
    )
    .await?;
    let (media_type, data, width, height) =
        images::checked(&reply).with_context(|| match reply["error"].as_str() {
            Some(code) => format!("refused: {code}: {}", reply["message"]),
            None => "unusable picture".to_owned(),
        })?;
    let side = |key: &str, fallback: u32| {
        reply[key]
            .as_u64()
            .and_then(|side| u32::try_from(side).ok())
            .unwrap_or(fallback)
    };
    Ok(Rendition {
        media_type,
        data,
        original: (
            side("original_width", width),
            side("original_height", height),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing_trusts_content_over_names() {
        let png = b"\x89PNG\r\n\x1a\n0000";
        assert_eq!(sniff(png, 12, "notes.txt"), (Kind::Image, "image/png"));
        assert_eq!(
            sniff(b"%PDF-1.7 ...", 12, "a.bin"),
            (Kind::Pdf, "application/pdf")
        );
        assert_eq!(sniff(b"a,b\n1,2\n", 8, "t.csv"), (Kind::Text, "text/csv"));
        assert_eq!(
            sniff("你好".as_bytes(), 6, "hello"),
            (Kind::Text, "text/plain")
        );
        // Binary, truncated or too large text is a plain file.
        assert_eq!(sniff(b"a\0b", 3, "x.txt").0, Kind::File);
        assert_eq!(sniff(b"abc", 40_000, "big.csv"), (Kind::File, "text/csv"));
        assert_eq!(sniff(&[0xFF, 0xFE, 0x00], 3, "x").0, Kind::File);
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg'/>";
        assert_eq!(
            sniff(svg, svg.len() as u64, "a.svg"),
            (Kind::Text, "text/plain")
        );
        assert_eq!(sniff(b"PK\x03\x04", 50_000, "a.xlsx").0, Kind::File);
    }

    #[test]
    fn names_are_cleaned_bounded_and_unique() {
        assert_eq!(display_name("  report\n.pdf "), Some("report.pdf".into()));
        assert_eq!(display_name(" \t "), None);
        assert_eq!(path_name("a/b\\c.txt", |_| false), "a_b_c.txt");
        assert_eq!(path_name(".env", |_| false), "_env");
        let taken = ["chart.png", "chart (2).png"];
        assert_eq!(
            path_name("chart.png", |n| taken.contains(&n)),
            "chart (3).png"
        );
        let long = format!("{}.csv", "数".repeat(100));
        let name = path_name(&long, |_| false);
        assert!(
            name.len() <= MAX_PATH_NAME && name.ends_with(".csv"),
            "{name}"
        );
        let taken = path_name(&long, |n| n == name);
        assert!(
            taken.len() <= MAX_PATH_NAME && taken.ends_with(" (2).csv"),
            "{taken}"
        );
    }
}
