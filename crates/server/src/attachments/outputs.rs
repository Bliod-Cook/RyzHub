//! Files a model hands to the member by linking to them in its reply, as in
//! `[report](sandbox:/workspace/report.pdf)`. When the reply completes, each linked file is
//! copied out of the sandbox into the conversation owner's library and bound to the
//! conversation: it outlives the sandbox, and later sandboxes find it under `/attachments`.
use anyhow::Context;
use ryzhub_execution::HostCommand;
use ryzhub_proto::{AnswerDetail, Attachment, Item, ItemDetail, SharedFile, item_detail};
use uuid::Uuid;

use super::{
    bind, display_name,
    ingest::{Spool, ingest},
    limit,
};
use crate::{
    app::App,
    error::{Code, code_of},
    store::{Lease, Tx, effective_policy, load_item, upsert_item},
};

/// Files one reply can hand over.
pub const MAX_PER_ANSWER: usize = 20;

/// How models learn to hand files over.
pub const INSTRUCTIONS: &str = "To give the user a file, save it under /workspace and link to it in your reply, e.g. [report.pdf](sandbox:/workspace/report.pdf). When the reply is complete the file is copied to the user's file library, where they can preview and download it. Files under /attachments can be linked the same way.";

/// Whether the files `item` links to were handed over already.
pub fn handed_over(item: &Item) -> bool {
    matches!(
        &item.detail,
        Some(ItemDetail { kind: Some(item_detail::Kind::Answer(answer)) }) if !answer.files.is_empty()
    )
}

/// Record on the reply `item` the files it handed over, keeping what it cites.
pub fn hand_over(item: &mut Item, files: Vec<SharedFile>) {
    item.revision += 1;
    let mut answer = match item.detail.take().and_then(|detail| detail.kind) {
        Some(item_detail::Kind::Answer(answer)) => answer,
        _ => AnswerDetail::default(),
    };
    answer.files = files;
    item.detail = Some(ItemDetail {
        kind: Some(item_detail::Kind::Answer(answer)),
    });
}

/// What a reply links to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A path in the sandbox, relative to `/workspace`.
    Workspace(String),
    /// A file already in the conversation, by its name under `/attachments`.
    Attachment(String),
}

/// The files `text` links to, in order and each once, as `(link, target)`: the link is the
/// destination without `sandbox:` and percent-decoded, `/workspace/<path>` or
/// `/attachments/<name>`, which is how clients match what was tapped. Links in code spans and
/// fenced blocks are code, not links.
pub fn references(text: &str) -> Vec<(String, Target)> {
    let prose = without_code(text);
    let mut found: Vec<(String, Target)> = Vec::new();
    let mut rest = prose.as_str();
    while let Some(start) = rest.find("](") {
        rest = &rest[start + 2..];
        let Some((link, target)) = target(destination(rest)) else {
            continue;
        };
        if !found.iter().any(|(seen, _)| *seen == link) {
            found.push((link, target));
            if found.len() == MAX_PER_ANSWER {
                break;
            }
        }
    }
    found
}

/// A link destination: inside `<…>`, or up to whitespace or its closing parenthesis.
fn destination(text: &str) -> &str {
    if let Some(inner) = text.strip_prefix('<') {
        return inner.find('>').map_or("", |end| &inner[..end]);
    }
    let mut depth = 0usize;
    for (index, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return &text[..index],
            ')' => depth -= 1,
            c if c.is_whitespace() => return &text[..index],
            _ => {}
        }
    }
    text
}

fn target(destination: &str) -> Option<(String, Target)> {
    let link = decode(destination.strip_prefix("sandbox:").unwrap_or(destination));
    if link.contains('\0') {
        return None;
    }
    if let Some(path) = link.strip_prefix("/workspace/") {
        let target = Target::Workspace(path.to_owned());
        return (!path.is_empty()).then_some((link, target));
    }
    let name = link.strip_prefix("/attachments/")?;
    let target = Target::Attachment(name.to_owned());
    (!name.is_empty() && !name.contains('/')).then_some((link, target))
}

/// `text` with `%XX` escapes decoded; unchanged unless that gives UTF-8.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| text.to_owned())
}

/// `text` with fenced blocks and code spans blanked out.
fn without_code(text: &str) -> String {
    let mut prose = String::with_capacity(text.len());
    let mut fenced = false;
    for line in text.split_inclusive('\n') {
        let start = line.trim_start();
        if start.starts_with("```") || start.starts_with("~~~") {
            fenced = !fenced;
            prose.push('\n');
            continue;
        }
        if fenced {
            prose.push('\n');
            continue;
        }
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            prose.push_str(&rest[..open]);
            let ticks = rest[open..].bytes().take_while(|b| *b == b'`').count();
            let after = &rest[open + ticks..];
            match after.find(&"`".repeat(ticks)) {
                Some(close) => rest = &after[close + ticks..],
                None => {
                    prose.push_str(&rest[open..open + ticks]);
                    rest = after;
                }
            }
        }
        prose.push_str(rest);
    }
    prose
}

/// The run's completed replies that link to files not handed over yet.
pub async fn unhanded(tx: &mut Tx<'_>, org: Uuid, run: Uuid) -> anyhow::Result<Vec<Item>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM items WHERE org_id=$1 AND run_id=$2 AND kind='assistant' AND state='completed'
         ORDER BY first_seq",
    )
    .bind(org)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    let mut replies = Vec::new();
    for id in ids {
        let item = load_item(tx, org, id).await?;
        if !handed_over(&item) && !references(&item.text).is_empty() {
            replies.push(item);
        }
    }
    Ok(replies)
}

/// What `texts` link to, each once, at most [`MAX_PER_ANSWER`].
pub fn gather<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<(String, Target)> {
    let mut targets: Vec<(String, Target)> = Vec::new();
    for (link, target) in texts.into_iter().flat_map(references) {
        if targets.len() < MAX_PER_ANSWER && !targets.iter().any(|(seen, _)| *seen == link) {
            targets.push((link, target));
        }
    }
    targets
}

/// Record on each of the run's replies that links to files the files `shared` holds for its
/// links; a link it lacks could not be handed over.
pub async fn record(
    tx: &mut Tx<'_>,
    lease: &Lease,
    run: Uuid,
    shared: &[SharedFile],
) -> anyhow::Result<()> {
    for mut item in unhanded(tx, lease.org_id, run).await? {
        let files = references(&item.text)
            .into_iter()
            .map(|(link, _)| {
                shared
                    .iter()
                    .find(|file| file.link == link)
                    .cloned()
                    .unwrap_or(SharedFile {
                        link,
                        file: None,
                        error: "FILE_UNAVAILABLE".into(),
                    })
            })
            .collect();
        hand_over(&mut item, files);
        upsert_item(tx, lease, item).await?;
    }
    Ok(())
}

/// Publish the files a reply of the run under `lease` links to. `sandbox` is the run's running
/// sandbox, if it has one. This never fails the reply: a file that cannot be published says
/// why instead.
pub async fn publish(
    app: &App,
    lease: &Lease,
    sandbox: Option<Uuid>,
    targets: &[(String, Target)],
) -> Vec<SharedFile> {
    let owner = owner(app, lease).await.map_err(failure);
    let mut shared = Vec::with_capacity(targets.len());
    for (link, target) in targets {
        let file = match (&owner, target) {
            (Err(code), _) => Err(*code),
            (Ok(owner), Target::Attachment(name)) => existing(app, lease, owner.user, name)
                .await
                .map_err(failure),
            (Ok(_), Target::Workspace(_)) if sandbox.is_none() => Err("FILE_NOT_FOUND"),
            (Ok(owner), Target::Workspace(path)) => {
                let sandbox = sandbox.unwrap_or_default();
                from_sandbox(app, lease, sandbox, path, owner)
                    .await
                    .map_err(failure)
            }
        };
        shared.push(match file {
            Ok(file) => SharedFile {
                link: link.clone(),
                file: Some(file),
                error: String::new(),
            },
            Err(code) => SharedFile {
                link: link.clone(),
                file: None,
                error: code.into(),
            },
        });
    }
    shared
}

/// Whose library published files join, and its limits.
struct Owner {
    user: Uuid,
    limit: u64,
    quota: Option<u64>,
}

async fn owner(app: &App, lease: &Lease) -> anyhow::Result<Owner> {
    let mut tx = app.store.tx(lease.org_id).await?;
    // Conversations are private: the files go to the one member who has it.
    let user: Uuid =
        sqlx::query_scalar("SELECT creator_id FROM conversations WHERE org_id=$1 AND id=$2")
            .bind(lease.org_id)
            .bind(lease.conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    let policy = effective_policy(&mut tx, &app.config().policy, lease.org_id).await?;
    tx.commit().await?;
    Ok(Owner {
        user,
        limit: limit(&policy),
        quota: policy.attachment_quota_bytes,
    })
}

/// A file already in the conversation, by its name under `/attachments`.
async fn existing(app: &App, lease: &Lease, owner: Uuid, name: &str) -> anyhow::Result<Attachment> {
    let mut tx = app.store.tx(lease.org_id).await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT f.attachment_id FROM conversation_files f
         JOIN attachments a ON a.org_id=f.org_id AND a.id=f.attachment_id
         WHERE f.org_id=$1 AND f.conversation_id=$2 AND f.path_name=$3 AND a.state='ready'",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let id = id.ok_or_else(|| ryzhub_execution::Rejected("FILE_NOT_FOUND".into()))?;
    let (_, file) = bind(&mut tx, lease, owner, &[id.to_string()])
        .await?
        .pop()
        .context("bound file")?;
    tx.commit().await?;
    Ok(file)
}

/// A file of the sandbox's `/workspace`, added to the owner's library and the conversation.
async fn from_sandbox(
    app: &App,
    lease: &Lease,
    sandbox: Uuid,
    path: &str,
    owner: &Owner,
) -> anyhow::Result<Attachment> {
    let command = HostCommand::WorkspaceFile {
        sandbox_id: sandbox,
        exec_epoch: lease.epoch as u64,
        path: path.to_owned(),
    };
    let socket = crate::sandbox::endpoint(app, lease.org_id, sandbox).await?;
    let (size, reader) = ryzhub_execution::download(&socket, &command).await?;
    let spool = Spool::read(reader, size, owner.limit).await?;
    let name = path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .and_then(display_name)
        .unwrap_or_else(|| "file".into());
    // The same file linked again, or published again after a retry, is the entry it became.
    let mut tx = app.store.tx(lease.org_id).await?;
    let published: Option<Uuid> = sqlx::query_scalar(
        "SELECT a.id FROM conversation_files f
         JOIN attachments a ON a.org_id=f.org_id AND a.id=f.attachment_id
         WHERE f.org_id=$1 AND f.conversation_id=$2 AND a.direction='output' AND a.state='ready'
           AND a.name=$3 AND a.sha256=$4
         LIMIT 1",
    )
    .bind(lease.org_id)
    .bind(lease.conversation_id)
    .bind(&name)
    .bind(&spool.sha256)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let id = match published {
        Some(id) => id,
        None => {
            ingest(
                app,
                lease.org_id,
                owner.user,
                &name,
                "output",
                &spool,
                owner.quota,
            )
            .await?
        }
    };
    let mut tx = app.store.tx(lease.org_id).await?;
    let (_, file) = bind(&mut tx, lease, owner.user, &[id.to_string()])
        .await?
        .pop()
        .context("bound file")?;
    tx.commit().await?;
    Ok(file)
}

/// The code a failed file reports.
fn failure(error: anyhow::Error) -> &'static str {
    const REFUSALS: [&str; 4] = [
        "INVALID_PATH",
        "FILE_NOT_FOUND",
        "NOT_A_FILE",
        "FILE_TOO_LARGE",
    ];
    if let Some(ryzhub_execution::Rejected(code)) = error.downcast_ref()
        && let Some(known) = REFUSALS.iter().find(|known| *known == code)
    {
        return known;
    }
    match code_of(&error) {
        Some(Code::AttachmentTooLarge) => "FILE_TOO_LARGE",
        Some(Code::AttachmentQuotaExceeded) => "ATTACHMENT_QUOTA_EXCEEDED",
        _ => {
            tracing::warn!(error = format!("{error:#}"), "file not published");
            "FILE_UNAVAILABLE"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(text: &str) -> Vec<String> {
        references(text).into_iter().map(|(link, _)| link).collect()
    }

    #[test]
    fn replies_link_files_in_the_sandbox_and_the_conversation() {
        let text = "Here is [the report](sandbox:/workspace/out/report.pdf) and \
                    [data](/workspace/data.csv \"title\"), again [it](sandbox:/workspace/out/report.pdf), \
                    [spaced](<sandbox:/workspace/my file.txt>), [escaped](sandbox:/workspace/a%20b.txt), \
                    [input](sandbox:/attachments/notes.csv), [web](https://example.com/x.pdf), \
                    [nested](/attachments/a/b), [empty](sandbox:/workspace/).";
        assert_eq!(
            references(text),
            vec![
                (
                    "/workspace/out/report.pdf".into(),
                    Target::Workspace("out/report.pdf".into())
                ),
                (
                    "/workspace/data.csv".into(),
                    Target::Workspace("data.csv".into())
                ),
                (
                    "/workspace/my file.txt".into(),
                    Target::Workspace("my file.txt".into())
                ),
                (
                    "/workspace/a b.txt".into(),
                    Target::Workspace("a b.txt".into())
                ),
                (
                    "/attachments/notes.csv".into(),
                    Target::Attachment("notes.csv".into())
                ),
            ]
        );
    }

    #[test]
    fn code_is_not_a_link() {
        let text = "Run `cat [x](sandbox:/workspace/a.txt)`.\n\
                    ```md\n[y](sandbox:/workspace/b.txt)\n```\n\
                    ``[z](sandbox:/workspace/c.txt)`` but [real](sandbox:/workspace/d(1).txt)";
        // Balanced parentheses belong to the destination.
        assert_eq!(links(text), ["/workspace/d(1).txt"]);
    }

    #[test]
    fn a_reply_hands_over_at_most_twenty_files() {
        let text: String = (0..30)
            .map(|i| format!("[f{i}](sandbox:/workspace/f{i}.txt) "))
            .collect();
        assert_eq!(references(&text).len(), MAX_PER_ANSWER);
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%E4%BD%A0"), "你");
        assert_eq!(decode("%FF"), "%FF");
    }
}
