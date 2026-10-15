//! Knowledge files from documents: text read in a process of its own, the document kept and
//! released with the last file that holds it.
use super::*;

/// A one-page PDF showing `text` in a standard font.
fn pdf(text: &str) -> Vec<u8> {
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

fn docx(paragraphs: &[&str]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
        .collect();
    let xml = format!(
        concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            "<w:body>{body}</w:body></w:document>"
        ),
        body = body
    );
    let mut out = std::io::Cursor::new(vec![]);
    let mut zip = zip::ZipWriter::new(&mut out);
    zip.start_file(
        "word/document.xml",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(xml.as_bytes()).unwrap();
    zip.finish().unwrap();
    out.into_inner()
}

async fn upload(
    h: &Harness,
    user: &Value,
    project: Uuid,
    name: &str,
    body: Vec<u8>,
) -> anyhow::Result<(u16, Value)> {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/orgs/{}/projects/{project}/files/upload",
            h.base,
            id(user, "org_id")
        ))
        .query(&[("name", name)])
        .bearer_auth(user["token"].as_str().unwrap())
        .body(body)
        .send()
        .await?;
    Ok((response.status().as_u16(), response.json().await?))
}

#[tokio::test]
#[ignore = "requires PostgreSQL/NATS"]
async fn documents_become_knowledge_files_and_keep_their_source() -> anyhow::Result<()> {
    let h = Harness::new().await?;
    let user = h.user().await?;
    let org = id(&user, "org_id");
    let token = user["token"].as_str().unwrap().to_owned();
    let client = reqwest::Client::new();
    let project: Value = client
        .post(format!("{}/orgs/{org}/projects", h.base))
        .bearer_auth(&token)
        .json(&json!({"name":"Docs","defaults":{"mode":"chat"}}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let project = id(&project, "id");
    let report = pdf("Quarterly plan 2026");
    let (status, saved) = upload(&h, &user, project, "plan.pdf", report.clone()).await?;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["file"]["source_type"], "application/pdf");
    let (status, saved) = upload(
        &h,
        &user,
        project,
        "Notes.docx",
        docx(&["First &amp; second", "第二段"]),
    )
    .await?;
    assert_eq!(status, 200, "{saved}");
    let (status, _) = upload(&h, &user, project, "readme.md", "# Plain".into()).await?;
    assert_eq!(status, 200);
    let files: Vec<Value> = client
        .get(format!("{}/orgs/{org}/projects/{project}/files", h.base))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let by_name = |name: &str| files.iter().find(|f| f["name"] == name).cloned().unwrap();
    assert_eq!(by_name("plan.pdf")["content"], "Quarterly plan 2026");
    assert_eq!(by_name("Notes.docx")["content"], "First & second\n第二段");
    assert_eq!(by_name("readme.md")["content"], "# Plain");
    assert!(
        by_name("readme.md")["source_sha256"].is_null(),
        "text is its own source"
    );

    // The document comes back as it was sent.
    let plan = id(&by_name("plan.pdf"), "id");
    let source = client
        .get(format!(
            "{}/orgs/{org}/projects/{project}/files/{plan}/source",
            h.base
        ))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(
        source.headers()["content-type"],
        "application/octet-stream",
        "never rendered by the browser"
    );
    assert_eq!(source.bytes().await?.as_ref(), report.as_slice());
    let other = h.user().await?;
    let denied = client
        .get(format!(
            "{}/orgs/{org}/projects/{project}/files/{plan}/source",
            h.base
        ))
        .bearer_auth(other["token"].as_str().unwrap())
        .send()
        .await?;
    assert!(!denied.status().is_success());

    // Refused: what is not a document, and a document that cannot be read.
    let (status, refused) = upload(
        &h,
        &user,
        project,
        "tool.bin",
        vec![0x7f, b'E', b'L', b'F', 0, 1],
    )
    .await?;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("UNSUPPORTED_PROJECT_FILE"))
    );
    let (status, refused) = upload(
        &h,
        &user,
        project,
        "broken.pdf",
        b"%PDF-1.4\ngarbage".to_vec(),
    )
    .await?;
    assert_eq!(
        (status, refused["error"].as_str()),
        (400, Some("DOCUMENT_UNREADABLE"))
    );

    // A library file with the same content shares the object: removing the knowledge file
    // keeps it for the library, and removing that too deletes it.
    let sha = by_name("plan.pdf")["source_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let store =
        ryzhub_server::workspace::blob::BlobStore::configured(&h.app.config.work.storage_root)?
            .scoped(org)
            .within("attachments");
    assert!(store.get(&sha).await.is_ok());
    let library = h.upload(&user, "plan.pdf", report.clone()).await?;
    client
        .delete(format!(
            "{}/orgs/{org}/projects/{project}/files/{plan}",
            h.base
        ))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?;
    assert!(store.get(&sha).await.is_ok(), "the library still holds it");
    client
        .delete(format!(
            "{}/orgs/{org}/attachments/{}",
            h.base,
            library["id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?;
    ryzhub_server::attachments::gc::collect_org(&h.app, org).await?;
    assert!(store.get(&sha).await.is_err(), "gone with the last holder");

    // Replacing a document releases the one it replaced.
    let notes = by_name("Notes.docx")["source_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(store.get(&notes).await.is_ok());
    let (status, _) = upload(&h, &user, project, "Notes.docx", docx(&["Rewritten"])).await?;
    assert_eq!(status, 200);
    assert!(store.get(&notes).await.is_err());
    h.close().await
}
