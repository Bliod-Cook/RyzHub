use super::*;

fn files(entries: &[(&str, &str)]) -> Files {
    entries
        .iter()
        .map(|(path, text)| (path.to_string(), text.as_bytes().to_vec()))
        .collect()
}

fn agent_plugin() -> Files {
    let plugin = json!({
        "$schema": PLUGIN_SCHEMA,
        "name": "acme-tracker",
        "version": "1.2.0",
        "description": "Acme tickets",
        "extensions": {
            NAMESPACE: {
                "permissions": {"deny": ["purge"]},
                "commands": [{"name": "triage", "title": "Triage", "prompt": "app.ryzhub/commands/triage.md",
                              "params": {"project": {"type": "string"}}}],
                "ui": {"mcpApps": true},
                "userConfig": {"apiToken": {"type": "string", "secret": true, "header": "Authorization", "prefix": "Bearer "}},
            },
            "com.other.vendor": {"anything": true},
        },
    });
    let mcp = json!({"$schema": MCP_SCHEMA, "mcpServers": {
        "acme": {"type": "streamable-http", "url": "https://mcp.acme.example/mcp"},
        "local": {"type": "stdio", "command": "acme-mcp"},
    }});
    files(&[
        ("plugin.json", &plugin.to_string()),
        ("mcp.json", &mcp.to_string()),
        (
            "skills/triage/SKILL.md",
            "---\nname: triage\ndescription: Sort new tickets\n---\nRead each ticket.",
        ),
        ("skills/triage/checklist.md", "1. priority"),
        (
            "app.ryzhub/commands/triage.md",
            "Triage the tickets of {{project}}.",
        ),
    ])
}

fn zipped(files: &Files) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
    for (path, bytes) in files {
        // A host's repository archive puts everything in one folder.
        zip.start_file(
            format!("acme-main/{path}"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn an_agent_plugin_with_the_platform_extension() {
    let unpacked = unpack(&zipped(&agent_plugin())).unwrap();
    assert!(
        unpacked.contains_key("plugin.json"),
        "{:?}",
        unpacked.keys()
    );
    let manifest = parse(&unpacked).unwrap();
    assert_eq!(
        (manifest.name.as_str(), manifest.format.as_str()),
        ("acme-tracker", "agent-plugins")
    );
    assert_eq!(manifest.servers.len(), 2);
    assert!(manifest.servers[0].enabled);
    // A stdio server is said, not run.
    assert!(!manifest.servers[1].enabled);
    assert_eq!(manifest.warnings.len(), 1);
    assert_eq!(manifest.skills[0].files["checklist.md"], "1. priority");
    assert_eq!(
        manifest.commands[0].prompt,
        "Triage the tickets of {{project}}."
    );
    assert!(manifest.permissions.deny.contains("purge"));
    assert!(manifest.ui.mcp_apps);
    assert!(manifest.user_config["apiToken"].secret);
    let rendered = render(
        &manifest.commands[0],
        json!({"project": "alpha"}).as_object().unwrap(),
    )
    .unwrap();
    assert_eq!(rendered, "Triage the tickets of alpha.");
    assert!(
        render(
            &manifest.commands[0],
            json!({"other": 1}).as_object().unwrap()
        )
        .is_err()
    );
    assert!(
        render(
            &manifest.commands[0],
            json!({"project": 3}).as_object().unwrap()
        )
        .is_err()
    );
}

#[test]
fn the_standard_is_strict_where_it_says_so() {
    let mut extra = agent_plugin();
    let mut plugin: Value = serde_json::from_slice(&extra["plugin.json"]).unwrap();
    plugin["commands"] = json!("not here");
    extra.insert("plugin.json".into(), plugin.to_string().into_bytes());
    assert!(parse(&extra).is_err());
    let mut named = agent_plugin();
    let mut plugin: Value = serde_json::from_slice(&named["plugin.json"]).unwrap();
    plugin["name"] = json!("Acme Tracker");
    named.insert("plugin.json".into(), plugin.to_string().into_bytes());
    assert!(parse(&named).is_err());
    let mut outside = agent_plugin();
    let mut plugin: Value = serde_json::from_slice(&outside["plugin.json"]).unwrap();
    plugin["extensions"][NAMESPACE]["commands"][0]["prompt"] = json!("../secrets");
    outside.insert("plugin.json".into(), plugin.to_string().into_bytes());
    assert!(parse(&outside).is_err());
    // Permissions are denials; other rules are refused rather than dropped.
    let mut ruled = agent_plugin();
    let mut plugin: Value = serde_json::from_slice(&ruled["plugin.json"]).unwrap();
    plugin["extensions"][NAMESPACE]["permissions"] = json!({"tools": {"default": "deny"}});
    ruled.insert("plugin.json".into(), plugin.to_string().into_bytes());
    assert!(parse(&ruled).is_err());
}

#[test]
fn archives_cannot_reach_outside_the_package() {
    let mut builder = tar::Builder::new(vec![]);
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o644);
    header.set_cksum();
    // `append_data` refuses such paths itself; a hostile archive is written raw.
    let mut raw = header.clone();
    raw.as_mut_bytes()[..9].copy_from_slice(b"../escape");
    raw.set_cksum();
    builder.append(&raw, &b"x"[..]).unwrap();
    let archive = builder.into_inner().unwrap();
    assert!(unpack(&archive).is_err());
    assert!(clean("/etc/passwd").is_err());
    assert!(clean("a/../../b").is_err());
    assert_eq!(clean("./a/b").unwrap().as_deref(), Some("a/b"));
}

#[test]
fn claude_code_plugins_are_converted() {
    let plugin = json!({"name": "Review-Kit", "version": "0.3.0", "description": "Reviews"});
    let mcp = json!({"mcpServers": {"docs": {"type": "http", "url": "https://docs.example.com/mcp"},
                                    "local": {"command": "node", "args": ["x.js"]}}});
    let package = files(&[
        (".claude-plugin/plugin.json", &plugin.to_string()),
        (".mcp.json", &mcp.to_string()),
        (
            "commands/review.md",
            "---\ndescription: Review a change\nargument-hint: <path>\n---\nReview $ARGUMENTS carefully.",
        ),
        (
            "skills/style/SKILL.md",
            "---\nname: style\ndescription: House style\n---\nUse tabs.",
        ),
        ("agents/helper.md", "helper"),
    ]);
    let manifest = parse(&package).unwrap();
    assert_eq!(
        (manifest.name.as_str(), manifest.format.as_str()),
        ("review-kit", "claude-code")
    );
    assert_eq!(
        manifest.commands[0].prompt,
        "Review {{arguments}} carefully."
    );
    assert_eq!(
        manifest.commands[0].params["arguments"]["description"],
        "<path>"
    );
    assert!(
        manifest
            .servers
            .iter()
            .any(|s| s.url == "https://docs.example.com/mcp" && s.enabled)
    );
    assert!(
        manifest
            .servers
            .iter()
            .any(|s| s.name == "local" && !s.enabled)
    );
    assert_eq!(manifest.warnings.len(), 2, "{:?}", manifest.warnings);
}

#[test]
fn the_content_hash_leaves_out_the_signature() {
    let mut package = agent_plugin();
    let hash = content_hash(&package);
    package.insert(
        format!("{NAMESPACE}/signature.json"),
        br#"{"key":"k","signature":"c2ln"}"#.to_vec(),
    );
    assert_eq!(content_hash(&package), hash);
    assert_eq!(
        parse(&package).unwrap().signature,
        Some(("k".into(), "c2ln".into()))
    );
    package.insert(
        "skills/triage/SKILL.md".into(),
        b"---\nname: triage\ndescription: x\n---\nother".to_vec(),
    );
    assert_ne!(content_hash(&package), hash);
}

#[test]
fn a_version_asking_for_more_is_noticed() {
    let manifest = parse(&agent_plugin()).unwrap();
    assert!(!manifest.widened_by(&manifest));
    let mut looser = manifest.clone();
    looser.permissions.deny.clear();
    assert!(manifest.widened_by(&looser));
    let mut elsewhere = manifest.clone();
    elsewhere.servers[0].url = "https://other.example/mcp".into();
    assert!(manifest.widened_by(&elsewhere));
    let mut stricter = manifest.clone();
    stricter.permissions.deny.insert("lookup".into());
    assert!(!manifest.widened_by(&stricter));
    assert_eq!(
        minimal("https://mcp.example.com/mcp", None).unwrap().name,
        "mcp.example.com"
    );
}
