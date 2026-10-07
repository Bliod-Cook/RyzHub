//! Images and documents in Chat model context.
//!
//! Two sources: images the model looks at with `view_image` (decoded and scaled inside the
//! sandbox, kept per conversation in `chat_images`), and files of the member's library that a
//! message carries (see `crate::attachments`). Model context holds references instead of the
//! bytes, so snapshots and forks stay small. [`inline`] turns them into provider content for
//! each request, newest first within a budget; what does not fit, what the model cannot read
//! and what was deleted becomes a short note.
use anyhow::Context;
use base64::Engine;
use ryzhub_execution::GuestCommand;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

use crate::{
    app::App,
    error::Code,
    sandbox,
    store::{Lease, Tx},
    workspace::attachment_stores,
};

/// Longest side the model sees: the per-image limit of a Messages request with many images.
pub const MAX_EDGE: u32 = 2000;
/// Largest kept image, well under the providers' per-image limits once base64-encoded.
pub const MAX_BYTES: usize = 3 * 1024 * 1024;
/// Media re-sent with each request, newest first; earlier ones become a note.
const CONTEXT_IMAGES: usize = 20;
const CONTEXT_DOCUMENTS: usize = 4;
const CONTEXT_BYTES: u64 = 16 * 1024 * 1024;
/// Larger PDFs stay in the sandbox only.
const MAX_DOCUMENT: u64 = 10 * 1024 * 1024;
/// Text files inlined into one message, in total.
/// Text files a request carries in full, in bytes; older ones become notes.
const CONTEXT_TEXT: u64 = 256 * 1024;
/// Reference types in stored model context.
const VIEWED: &str = "ryzhub_image";
const LIBRARY: &str = "ryzhub_attachment";

/// Run `view_image`: the result names the stored image, never its bytes.
pub async fn view(
    app: &App,
    lease: &Lease,
    run: Uuid,
    sandbox: Uuid,
    args: &Value,
) -> anyhow::Result<Value> {
    let path = args["path"]
        .as_str()
        .filter(|path| !path.is_empty() && path.len() <= 4096)
        .context(Code::InvalidToolArguments)?;
    let command = GuestCommand::ReadImage {
        path: path.into(),
        max_edge: MAX_EDGE,
        max_bytes: MAX_BYTES,
    };
    let reply = sandbox::guest(app, lease, sandbox, command).await?;
    if let Some(error) = reply["error"].as_str() {
        let bounded = |text: &str, limit| text.chars().take(limit).collect::<String>();
        return Ok(json!({
            "error": bounded(error, 64),
            "message": bounded(reply["message"].as_str().unwrap_or_default(), 500),
            "path": path,
        }));
    }
    // The guest is not trusted: keep only a well-formed image within the limits.
    let Some((media_type, data, width, height)) = checked(&reply) else {
        return Ok(json!({"error": "invalid_image", "path": path}));
    };
    let sha256 = format!("{:x}", Sha256::digest(&data));
    let mut tx = crate::runs::guarded(&app.store, lease, run).await?;
    sqlx::query(
        "INSERT INTO chat_images(org_id,conversation_id,sha256,media_type,data,width,height)
         VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(&sha256)
    .bind(media_type)
    .bind(&data)
    .bind(width as i32)
    .bind(height as i32)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let original = |key: &str, fallback: u32| reply[key].as_u64().unwrap_or(fallback.into());
    Ok(json!({
        "path": path,
        "image": {
            "sha256": sha256,
            "media_type": media_type,
            "width": width,
            "height": height,
            "bytes": data.len(),
        },
        "original": {
            "width": original("original_width", width),
            "height": original("original_height", height),
        },
    }))
}

/// A rendered image as `picture` replies with it: well-formed, the type it claims and within
/// the limits. `None` for anything else.
pub(crate) fn checked(reply: &Value) -> Option<(&'static str, Vec<u8>, u32, u32)> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(reply["data"].as_str()?)
        .ok()?;
    let media_type = match reply["media_type"].as_str()? {
        "image/png" if data.starts_with(b"\x89PNG\r\n\x1a\n") => "image/png",
        "image/jpeg" if data.starts_with(&[0xFF, 0xD8, 0xFF]) => "image/jpeg",
        "image/webp" if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" => {
            "image/webp"
        }
        _ => return None,
    };
    let side = |key: &str| {
        reply[key]
            .as_u64()
            .filter(|side| (1..=MAX_EDGE as u64).contains(side))
    };
    let (width, height) = (side("width")?, side("height")?);
    (data.len() <= MAX_BYTES).then_some((media_type, data, width as u32, height as u32))
}

fn text_part(protocol: &str, text: String) -> Value {
    if protocol == "responses" {
        json!({"type": "input_text", "text": text})
    } else {
        json!({"type": "text", "text": text})
    }
}

/// A tool result as model context: the image reference first, then the result as text.
/// `None` for results without an image.
pub fn tool_content(protocol: &str, result: &Value) -> Option<Value> {
    let image = result.get("image")?;
    let reference = json!({
        "type": VIEWED,
        "sha256": image["sha256"],
        "bytes": image["bytes"],
        "path": result["path"],
    });
    Some(json!([reference, text_part(protocol, result.to_string())]))
}

/// A user message carrying files of the member's library: references to images and PDFs, a
/// note naming every file, references to small text files, then the text. Each request expands
/// the references (see [`inline`]), so a file deleted later leaves every request after. The
/// files are this conversation's references (see `attachments::bind`).
pub async fn user_message(
    tx: &mut Tx<'_>,
    org: Uuid,
    conversation: Uuid,
    protocol: &str,
    text: &str,
    files: &[Uuid],
    sandbox: bool,
) -> anyhow::Result<Value> {
    type Row = (
        Uuid,
        String,
        String,
        String,
        i64,
        Option<i32>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.id,a.name,a.media_type,a.kind,a.size,octet_length(a.model_data),f.path_name
         FROM attachments a LEFT JOIN conversation_files f
           ON f.org_id=a.org_id AND f.attachment_id=a.id AND f.conversation_id=$2
         WHERE a.org_id=$1 AND a.id=ANY($3) AND a.state='ready'",
    )
    .bind(org)
    .bind(conversation)
    .bind(files)
    .fetch_all(&mut **tx)
    .await?;
    let mut references = vec![];
    let mut lines = vec![];
    let mut texts = vec![];
    for id in files {
        let Some((_, name, media_type, kind, size, rendition, Some(path_name))) =
            rows.iter().find(|row| row.0 == *id).cloned()
        else {
            lines.push("- (a file that was deleted)".to_owned());
            continue;
        };
        let path = format!("/attachments/{path_name}");
        let shown = if sandbox { path.clone() } else { name.clone() };
        lines.push(format!(
            "- {shown} ({media_type}, {})",
            crate::attachments::human_size(size as u64)
        ));
        match kind.as_str() {
            "image" => references.push(json!({
                "type": LIBRARY, "id": id, "kind": "image", "path": path, "name": name,
                "bytes": rendition.unwrap_or_default(),
            })),
            "pdf" => references.push(json!({
                "type": LIBRARY, "id": id, "kind": "pdf", "path": path, "name": name, "bytes": size,
            })),
            "text" => texts.push(json!({
                "type": LIBRARY, "id": id, "kind": "text", "path": path, "name": name, "bytes": size,
            })),
            _ => {}
        }
    }
    let mut note = format!(
        "Attached files{}:\n{}",
        if sandbox {
            " (read-only in the sandbox)"
        } else {
            ""
        },
        lines.join("\n")
    );
    if sandbox {
        note.push_str("\nUse exec to work with them; view_image shows an image again.");
    }
    let mut content = references;
    content.push(text_part(protocol, note));
    content.extend(texts);
    if !text.trim().is_empty() {
        content.push(text_part(protocol, text.to_owned()));
    }
    Ok(json!({"role": "user", "content": content}))
}

/// How a request treats media.
pub struct Options {
    /// False for a compaction request: media become notes.
    pub media: bool,
    /// Input kinds the model reads (`image`, `pdf`).
    pub accepts: Vec<String>,
    /// Whether the conversation's files are in a sandbox the model can reach.
    pub sandbox: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Image,
    Document,
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Source {
    Viewed(String),
    Library(Uuid),
}

#[derive(Clone, Debug)]
struct Media {
    kind: Kind,
    source: Source,
    bytes: u64,
    path: String,
    name: String,
}

impl Media {
    fn of(part: &Value) -> Option<Self> {
        let text = |key: &str| part[key].as_str().unwrap_or_default().to_owned();
        let bytes = part["bytes"].as_u64().unwrap_or(u64::MAX);
        match part["type"].as_str()? {
            VIEWED => Some(Self {
                kind: Kind::Image,
                source: Source::Viewed(text("sha256")),
                bytes,
                path: text("path"),
                name: String::new(),
            }),
            LIBRARY => Some(Self {
                kind: match part["kind"].as_str() {
                    Some("pdf") => Kind::Document,
                    Some("text") => Kind::Text,
                    _ => Kind::Image,
                },
                source: Source::Library(part["id"].as_str()?.parse().ok()?),
                bytes,
                path: text("path"),
                name: text("name"),
            }),
            _ => None,
        }
    }
}

/// Why a reference is not sent as content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Left {
    Compacting,
    Unsupported,
    TooLarge,
    OverBudget,
}

/// Replace the media references in a request body with content or notes.
pub async fn inline(
    app: &App,
    org: Uuid,
    conversation: Uuid,
    protocol: &str,
    body: &mut Value,
    options: &Options,
) -> anyhow::Result<()> {
    let found: Vec<Media> = references(protocol, body)
        .into_iter()
        .filter_map(|part| Media::of(part))
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    let plan = budget(&found, options);
    let loaded = load(app, org, conversation, &found, &plan).await?;
    substitute(protocol, body, &plan, &loaded, options);
    Ok(())
}

/// Media references in user content and tool results, oldest first. Only the server builds
/// user-role content arrays; what the model writes is never read here.
fn references<'a>(protocol: &str, body: &'a mut Value) -> Vec<&'a mut Value> {
    let is_reference = |part: &Value| part["type"] == VIEWED || part["type"] == LIBRARY;
    let list = if protocol == "responses" {
        "input"
    } else {
        "messages"
    };
    let mut out = vec![];
    for item in body
        .get_mut(list)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let parts = if protocol == "responses" && item["type"] == "function_call_output" {
            "output"
        } else if item["role"] == "user" {
            "content"
        } else {
            continue;
        };
        for part in item
            .get_mut(parts)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if is_reference(part) {
                out.push(part);
            } else if part["type"] == "tool_result" {
                out.extend(
                    part.get_mut("content")
                        .and_then(Value::as_array_mut)
                        .into_iter()
                        .flatten()
                        .filter(|part| is_reference(part)),
                );
            }
        }
    }
    out
}

/// What each reference becomes: content (`None`) or a note, newest first while within budget.
fn budget(found: &[Media], options: &Options) -> Vec<Option<Left>> {
    let accepts = |kind: Kind| {
        let name = match kind {
            Kind::Image => "image",
            Kind::Document => "pdf",
            // Every model reads text.
            Kind::Text => return true,
        };
        options.accepts.iter().any(|accepted| accepted == name)
    };
    let (mut images, mut documents, mut bytes) = (0, 0, 0u64);
    let mut full = false;
    let (mut text, mut texts_full) = (0u64, false);
    let mut plan = vec![None; found.len()];
    for (index, media) in found.iter().enumerate().rev() {
        plan[index] = if !options.media {
            Some(Left::Compacting)
        } else if media.kind == Kind::Text {
            // Text has a budget of its own: it is not what fills a request with media.
            texts_full |= text.saturating_add(media.bytes) > CONTEXT_TEXT;
            if texts_full {
                Some(Left::OverBudget)
            } else {
                text += media.bytes;
                None
            }
        } else if !accepts(media.kind) {
            Some(Left::Unsupported)
        } else if media.kind == Kind::Document && media.bytes > MAX_DOCUMENT {
            Some(Left::TooLarge)
        } else {
            let (count, cap) = match media.kind {
                Kind::Document => (&mut documents, CONTEXT_DOCUMENTS),
                _ => (&mut images, CONTEXT_IMAGES),
            };
            full |= bytes.saturating_add(media.bytes) > CONTEXT_BYTES;
            if full || *count == cap {
                Some(Left::OverBudget)
            } else {
                *count += 1;
                bytes += media.bytes;
                None
            }
        };
    }
    plan
}

/// The bytes of every reference sent as content, with their media type. A missing entry was
/// deleted, or is no longer this conversation's.
async fn load(
    app: &App,
    org: Uuid,
    conversation: Uuid,
    found: &[Media],
    plan: &[Option<Left>],
) -> anyhow::Result<HashMap<Source, (String, Vec<u8>)>> {
    let wanted = || {
        found
            .iter()
            .zip(plan)
            .filter(|(_, left)| left.is_none())
            .map(|(media, _)| media)
    };
    let viewed: Vec<&str> = wanted()
        .filter_map(|media| match &media.source {
            Source::Viewed(sha256) => Some(sha256.as_str()),
            _ => None,
        })
        .collect();
    let library: Vec<Uuid> = wanted()
        .filter_map(|media| match media.source {
            Source::Library(id) => Some(id),
            _ => None,
        })
        .collect();
    let mut loaded = HashMap::new();
    if viewed.is_empty() && library.is_empty() {
        return Ok(loaded);
    }
    let mut tx = app.store.tx(org).await?;
    let images: Vec<(String, String, Vec<u8>)> = sqlx::query_as(
        "SELECT sha256,media_type,data FROM chat_images
         WHERE org_id=$1 AND conversation_id=$2 AND sha256=ANY($3)",
    )
    .bind(org)
    .bind(conversation)
    .bind(&viewed)
    .fetch_all(&mut *tx)
    .await?;
    for (sha256, media_type, data) in images {
        loaded.insert(Source::Viewed(sha256), (media_type, data));
    }
    // Only files the conversation still refers to: deleting a file removes its references.
    type File = (
        Uuid,
        String,
        Option<String>,
        Option<Vec<u8>>,
        String,
        Option<String>,
    );
    let files: Vec<File> = sqlx::query_as(
        "SELECT a.id,a.kind,a.model_media_type,a.model_data,a.sha256,a.text_content
         FROM attachments a JOIN conversation_files f
           ON f.org_id=a.org_id AND f.attachment_id=a.id AND f.conversation_id=$2
         WHERE a.org_id=$1 AND a.id=ANY($3) AND a.state='ready'",
    )
    .bind(org)
    .bind(conversation)
    .bind(&library)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut stores = None;
    for (id, kind, media_type, data, sha256, text) in files {
        let content = match (kind.as_str(), media_type, data) {
            ("image", Some(media_type), Some(data)) => (media_type, data),
            ("text", _, _) => match text {
                Some(text) => ("text/plain".to_owned(), text.into_bytes()),
                None => continue,
            },
            ("pdf", _, _) => {
                if stores.is_none() {
                    stores = Some(attachment_stores(app, org)?);
                }
                let (primary, secondary) = stores.as_ref().context("stores")?;
                let data = match primary.get(&sha256).await {
                    Ok(data) => data,
                    Err(error) => match secondary {
                        Some(secondary) => secondary.get(&sha256).await?,
                        None => return Err(error),
                    },
                };
                ("application/pdf".to_owned(), data)
            }
            _ => continue,
        };
        loaded.insert(Source::Library(id), content);
    }
    Ok(loaded)
}

fn substitute(
    protocol: &str,
    body: &mut Value,
    plan: &[Option<Left>],
    loaded: &HashMap<Source, (String, Vec<u8>)>,
    options: &Options,
) {
    let responses = protocol == "responses";
    for (part, left) in references(protocol, body).into_iter().zip(plan) {
        let Some(media) = Media::of(part) else {
            continue;
        };
        let path = media.path.as_str();
        let content = match left {
            None => loaded.get(&media.source),
            Some(_) => None,
        };
        *part = match (content, media.kind) {
            (Some((media_type, data)), Kind::Image) => {
                let data = base64::engine::general_purpose::STANDARD.encode(data);
                if responses {
                    json!({"type": "input_image", "image_url": format!("data:{media_type};base64,{data}")})
                } else {
                    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
                }
            }
            (Some((media_type, data)), Kind::Document) => {
                let data = base64::engine::general_purpose::STANDARD.encode(data);
                let name = if media.name.is_empty() {
                    "document.pdf"
                } else {
                    &media.name
                };
                if responses {
                    json!({"type": "input_file", "filename": name, "file_data": format!("data:{media_type};base64,{data}")})
                } else {
                    json!({"type": "document", "title": name, "source": {"type": "base64", "media_type": media_type, "data": data}})
                }
            }
            (Some((_, data)), Kind::Text) => {
                let shown = if options.sandbox { path } else { &media.name };
                let text = String::from_utf8_lossy(data);
                text_part(
                    protocol,
                    format!("<attachment path=\"{shown}\">\n{text}\n</attachment>"),
                )
            }
            (None, kind) => {
                let noun = match kind {
                    Kind::Image => "Image",
                    Kind::Document => "Document",
                    Kind::Text => "File",
                };
                let text = match (left, &media.source) {
                    (Some(Left::Compacting), _) => format!("[{noun} {path}]"),
                    (Some(Left::Unsupported), _) => format!(
                        "[{noun} {path}: this model cannot read it directly{}.]",
                        if options.sandbox {
                            "; the file is in the sandbox"
                        } else {
                            ""
                        }
                    ),
                    (Some(Left::TooLarge), _) => format!(
                        "[{noun} {path} is too large to show directly{}.]",
                        if options.sandbox {
                            "; the file is in the sandbox"
                        } else {
                            ""
                        }
                    ),
                    (Some(Left::OverBudget), _) if kind == Kind::Text => format!(
                        "[File {path} is not included here; there is too much attached text{}.]",
                        if options.sandbox {
                            ", but it is in the sandbox"
                        } else {
                            ""
                        }
                    ),
                    (Some(Left::OverBudget), _) if kind == Kind::Image && options.sandbox => {
                        format!(
                            "[Image {path} was shown earlier and is no longer attached; call view_image with this path to see it again.]"
                        )
                    }
                    (Some(Left::OverBudget), _) => {
                        format!("[{noun} {path} was shown earlier and is no longer attached.]")
                    }
                    (None, Source::Library(_)) => {
                        format!("[File {path} was deleted by the user.]")
                    }
                    (None, Source::Viewed(_)) => format!("[Image {path} is no longer available.]"),
                };
                text_part(protocol, text)
            }
        };
    }
}

/// Remove inline media a provider echoed back, e.g. in a compaction's output, so stored
/// context never keeps the bytes.
pub fn scrub(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::Object(object) => {
            let inline = ["image_url", "file_data"].iter().any(|key| {
                object
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.starts_with("data:"))
            }) || object
                .get("source")
                .is_some_and(|source| source["type"] == "base64");
            if inline {
                let responses =
                    object.contains_key("image_url") || object.contains_key("file_data");
                *value = text_part(
                    if responses { "responses" } else { "anthropic" },
                    "[media omitted]".into(),
                );
            } else {
                object.values_mut().for_each(scrub);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(sha256: &str, bytes: u64) -> Value {
        json!({"path": format!("/workspace/{sha256}.png"), "image": {"sha256": sha256, "media_type": "image/png", "width": 1, "height": 1, "bytes": bytes}})
    }

    fn options() -> Options {
        Options {
            media: true,
            accepts: vec!["image".into(), "pdf".into()],
            sandbox: true,
        }
    }

    fn library(id: Uuid, kind: &str, bytes: u64) -> Value {
        json!({"type": LIBRARY, "id": id, "kind": kind, "path": format!("/attachments/{kind}"), "name": format!("{kind}.bin"), "bytes": bytes})
    }

    #[test]
    fn references_follow_each_protocol_and_inline_newest_first() {
        let mut anthropic = json!({"messages": [
            {"role": "user", "content": "draw"},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": tool_content("anthropic", &result("old", 9 * 1024 * 1024)).unwrap()}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "b", "content": tool_content("anthropic", &result("new", 8 * 1024 * 1024)).unwrap()},
                {"type": "tool_result", "tool_use_id": "c", "content": tool_content("anthropic", &result("gone", 10)).unwrap()},
            ]},
            // What the model writes is never read as a reference.
            {"role": "assistant", "content": [{"type": "tool_use", "id": "d", "name": "exec", "input": {"type": VIEWED, "sha256": "new"}}, {"type": VIEWED, "sha256": "new"}]},
        ]});
        let found: Vec<Media> = references("anthropic", &mut anthropic)
            .into_iter()
            .filter_map(|part| Media::of(part))
            .collect();
        assert_eq!(
            found.iter().map(|m| m.bytes).collect::<Vec<_>>(),
            [9 * 1024 * 1024, 8 * 1024 * 1024, 10]
        );
        let plan = budget(&found, &options());
        assert_eq!(plan, [Some(Left::OverBudget), None, None]);
        let loaded = HashMap::from([
            (
                Source::Viewed("old".into()),
                ("image/png".to_owned(), vec![1]),
            ),
            (
                Source::Viewed("new".into()),
                ("image/jpeg".to_owned(), vec![2, 3]),
            ),
        ]);
        substitute("anthropic", &mut anthropic, &plan, &loaded, &options());
        let messages = &anthropic["messages"];
        assert!(
            messages[1]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("call view_image")
        );
        assert_eq!(
            messages[2]["content"][0]["content"][0],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AgM="}})
        );
        assert_eq!(messages[2]["content"][0]["content"][1]["type"], "text");
        assert!(
            messages[2]["content"][1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no longer available")
        );
        assert_eq!(messages[3]["content"][0]["input"]["type"], VIEWED);
        assert_eq!(messages[3]["content"][1]["type"], VIEWED);

        let mut responses = json!({"input": [
            {"type": "function_call", "call_id": "a", "name": "view_image", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "a", "output": tool_content("responses", &result("new", 2)).unwrap()},
        ]});
        substitute("responses", &mut responses, &[None], &loaded, &options());
        assert_eq!(
            responses["input"][1]["output"][0],
            json!({"type": "input_image", "image_url": "data:image/jpeg;base64,AgM="})
        );
        assert_eq!(responses["input"][1]["output"][1]["type"], "input_text");
        assert!(tool_content("responses", &json!({"stdout_head": "x"})).is_none());
    }

    #[test]
    fn library_files_in_user_messages_become_content_or_notes() {
        let (image, pdf, deleted) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let mut responses = json!({"input": [
            {"role": "user", "content": [library(image, "image", 10), library(pdf, "pdf", 20), library(deleted, "image", 5), {"type": "input_text", "text": "look"}]},
            {"type": "message", "role": "assistant", "content": [library(image, "image", 10)]},
        ]});
        let found: Vec<Media> = references("responses", &mut responses)
            .into_iter()
            .filter_map(|part| Media::of(part))
            .collect();
        assert_eq!(found.len(), 3, "assistant content is not read");
        let plan = budget(&found, &options());
        let loaded = HashMap::from([
            (Source::Library(image), ("image/png".to_owned(), vec![1])),
            (
                Source::Library(pdf),
                ("application/pdf".to_owned(), vec![2]),
            ),
        ]);
        substitute("responses", &mut responses, &plan, &loaded, &options());
        let content = &responses["input"][0]["content"];
        assert_eq!(
            content[0],
            json!({"type": "input_image", "image_url": "data:image/png;base64,AQ=="})
        );
        assert_eq!(
            content[1],
            json!({"type": "input_file", "filename": "pdf.bin", "file_data": "data:application/pdf;base64,Ag=="})
        );
        assert_eq!(
            content[2]["text"],
            "[File /attachments/image was deleted by the user.]"
        );
        assert_eq!(content[3]["text"], "look");

        let mut anthropic =
            json!({"messages": [{"role": "user", "content": [library(pdf, "pdf", 20)]}]});
        substitute("anthropic", &mut anthropic, &[None], &loaded, &options());
        assert_eq!(anthropic["messages"][0]["content"][0]["type"], "document");
        assert_eq!(
            anthropic["messages"][0]["content"][0]["source"]["media_type"],
            "application/pdf"
        );

        // What the model cannot read, what is too large and compaction requests get notes.
        let images_only = Options {
            accepts: vec!["image".into()],
            ..options()
        };
        let big = Media::of(&library(pdf, "pdf", MAX_DOCUMENT + 1)).unwrap();
        let small = Media::of(&library(image, "image", 1)).unwrap();
        assert_eq!(
            budget(&[big.clone(), small.clone()], &images_only),
            [Some(Left::Unsupported), None]
        );
        assert_eq!(budget(&[big], &options()), [Some(Left::TooLarge)]);
        let compacting = Options {
            media: false,
            ..options()
        };
        assert_eq!(budget(&[small], &compacting), [Some(Left::Compacting)]);
    }

    #[test]
    fn text_files_expand_in_each_request_until_deleted() {
        let (notes, gone) = (Uuid::now_v7(), Uuid::now_v7());
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "Attached files: ..."},
            library(notes, "text", 4),
            library(gone, "text", 4),
            {"type": "text", "text": "read them"},
        ]}]});
        let found: Vec<Media> = references("anthropic", &mut body)
            .into_iter()
            .filter_map(|part| Media::of(part))
            .collect();
        assert_eq!(found.len(), 2);
        let plan = budget(&found, &options());
        assert_eq!(plan, [None, None]);
        // Only what the conversation still has is loaded: the other was deleted.
        let loaded = HashMap::from([(
            Source::Library(notes),
            ("text/plain".to_owned(), b"a,b\n".to_vec()),
        )]);
        substitute("anthropic", &mut body, &plan, &loaded, &options());
        let content = &body["messages"][0]["content"];
        assert_eq!(
            content[1]["text"],
            "<attachment path=\"/attachments/text\">\na,b\n\n</attachment>"
        );
        assert_eq!(
            content[2]["text"],
            "[File /attachments/text was deleted by the user.]"
        );
        assert_eq!(content[3]["text"], "read them");

        // Text has its own budget, newest first; images do not take from it.
        let big = CONTEXT_TEXT / 2 + 1;
        let texts: Vec<Media> = (0..3)
            .map(|_| Media::of(&library(Uuid::now_v7(), "text", big)).unwrap())
            .collect();
        assert_eq!(
            budget(&texts, &options()),
            [Some(Left::OverBudget), Some(Left::OverBudget), None]
        );
        let compacting = Options {
            media: false,
            ..options()
        };
        assert_eq!(budget(&texts[..1], &compacting), [Some(Left::Compacting)]);
    }

    #[test]
    fn the_budget_caps_each_kind() {
        let images: Vec<Media> = (0..25)
            .map(|_| Media::of(&library(Uuid::now_v7(), "image", 1)).unwrap())
            .collect();
        let plan = budget(&images, &options());
        assert_eq!(
            plan.iter().filter(|left| left.is_none()).count(),
            CONTEXT_IMAGES
        );
        assert!(plan[4].is_some() && plan[5].is_none() && plan[24].is_none());
        let documents: Vec<Media> = (0..6)
            .map(|_| Media::of(&library(Uuid::now_v7(), "pdf", 1)).unwrap())
            .collect();
        let plan = budget(&documents, &options());
        assert_eq!(
            plan.iter().filter(|left| left.is_none()).count(),
            CONTEXT_DOCUMENTS
        );
    }

    #[test]
    fn scrubbing_removes_echoed_media() {
        let mut output = json!([
            {"role": "user", "content": [
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                {"type": "input_image", "image_url": "https://example.com/a.png"},
                {"type": "input_text", "text": "keep"},
            ]},
            {"type": "compaction", "encrypted_content": "opaque"},
        ]);
        scrub(&mut output);
        assert_eq!(
            output[0]["content"][0],
            json!({"type": "input_text", "text": "[media omitted]"})
        );
        assert_eq!(
            output[0]["content"][1]["image_url"],
            "https://example.com/a.png"
        );
        assert_eq!(output[1]["encrypted_content"], "opaque");
    }

    #[test]
    fn guest_replies_must_be_the_image_they_claim() {
        let encode = |data: &[u8]| base64::engine::general_purpose::STANDARD.encode(data);
        let png = encode(b"\x89PNG\r\n\x1a\nrest");
        let reply = json!({"media_type": "image/png", "data": png, "width": 10, "height": 5});
        assert_eq!(
            checked(&reply).map(|(media_type, _, w, h)| (media_type, w, h)),
            Some(("image/png", 10, 5))
        );
        assert!(
            checked(&json!({"media_type": "image/jpeg", "data": png, "width": 10, "height": 5}))
                .is_none()
        );
        assert!(
            checked(&json!({"media_type": "image/png", "data": png, "width": 4000, "height": 5}))
                .is_none()
        );
        assert!(checked(&json!({"media_type": "image/gif", "data": encode(b"GIF89a"), "width": 1, "height": 1})).is_none());
    }
}
