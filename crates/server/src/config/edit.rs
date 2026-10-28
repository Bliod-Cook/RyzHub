//! Changing the configuration file from the admin panel. Only the settings changed are
//! rewritten, so comments and order survive; a list of tables is replaced whole.
use anyhow::Context;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table};

use super::Config;

/// Settings that name an environment variable holding a model key, and whether the empty
/// name (the protocol's default) is allowed.
pub const KEY_ENVS: &[(&str, bool)] = &[
    ("llm.platform_key_env", false),
    ("llm.responses_key_env", false),
    ("llm.chat_completions_key_env", false),
    ("llm.gemini_key_env", false),
    ("search.embedding_key_env", true),
];

/// The variables model keys may be read from: the providers' defaults and those the file
/// names for model keys already, never the vault's keys or sign-in secrets. The panel writes
/// only names from here, so it cannot widen the set, nor point an upstream of its own at a
/// secret such as the vault key.
pub fn key_envs(config: &Config) -> BTreeSet<String> {
    let llm = &config.llm;
    let mut names: BTreeSet<String> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY"]
        .map(String::from)
        .into();
    names.extend(
        [
            &llm.platform_key_env,
            &llm.responses_key_env,
            &llm.chat_completions_key_env,
            &llm.gemini_key_env,
            &config.search.embedding_key_env,
        ]
        .into_iter()
        .chain(
            llm.models
                .iter()
                .flat_map(|m| &m.upstreams)
                .map(|u| &u.key_env),
        )
        .cloned(),
    );
    let secrets = std::iter::once(&llm.vault_key_env)
        .chain(llm.vault_retired_keys.values())
        .chain(config.auth.oidc.iter().map(|p| &p.client_secret_env))
        .chain(config.auth.github.iter().map(|g| &g.client_secret_env));
    for name in secrets {
        names.remove(name);
    }
    names.remove("");
    names
}

/// Whether `value`, for setting `path`, names only variables from `allowed`.
pub fn names_allowed_env(path: &str, value: &Value, allowed: &BTreeSet<String>) -> bool {
    let fits = |name: &Value, empty: bool| match name.as_str() {
        Some("") => empty,
        Some(name) => allowed.contains(name),
        None => name.is_null() && empty,
    };
    if path == "llm.models" {
        return value.as_array().into_iter().flatten().all(|route| {
            route["upstreams"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|upstream| fits(&upstream["key_env"], true))
        });
    }
    match KEY_ENVS.iter().find(|(env, _)| *env == path) {
        Some((_, empty)) => fits(value, *empty),
        None => true,
    }
}

/// `text` with every setting in `values` (dotted path → value; null removes it) set.
pub fn apply(text: &str, values: &BTreeMap<String, Value>) -> anyhow::Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    for (path, value) in values {
        let (parents, key) = match path.rsplit_once('.') {
            Some((parents, key)) => (Some(parents), key),
            None => (None, path.as_str()),
        };
        let mut table = doc.as_table_mut();
        for name in parents.into_iter().flat_map(|p| p.split('.')) {
            let entry = table
                .entry(name)
                .or_insert_with(|| Item::Table(Table::new()));
            table = entry
                .as_table_mut()
                .with_context(|| format!("{path}: {name} is not a table"))?;
        }
        match item(value)? {
            None => {
                table.remove(key);
            }
            Some(mut new) => match table.get_mut(key) {
                // In place, the key keeps the comments above it, and a value its comment.
                Some(old) => {
                    if let (Some(old), Some(new)) = (old.as_value(), new.as_value_mut()) {
                        *new.decor_mut() = old.decor().clone();
                    }
                    *old = new;
                }
                None => {
                    table.insert(key, new);
                }
            },
        }
    }
    Ok(doc.to_string())
}

/// A setting's TOML: lists of objects are arrays of tables, nested ones inline.
fn item(value: &Value) -> anyhow::Result<Option<Item>> {
    Ok(match value {
        Value::Null => None,
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let mut tables = ArrayOfTables::new();
            for object in items {
                let mut table = Table::new();
                for (key, value) in object.as_object().into_iter().flatten() {
                    if let Some(value) = inline(value)? {
                        table.insert(key, Item::Value(value));
                    }
                }
                tables.push(table);
            }
            Some(Item::ArrayOfTables(tables))
        }
        value => inline(value)?.map(Item::Value),
    })
}

fn inline(value: &Value) -> anyhow::Result<Option<toml_edit::Value>> {
    Ok(Some(match value {
        Value::Null => return Ok(None),
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64().context("number")?.into(),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                if let Some(value) = inline(item)? {
                    array.push(value);
                }
            }
            array.into()
        }
        Value::Object(fields) => {
            let mut table = InlineTable::new();
            for (key, value) in fields {
                if let Some(value) = inline(value)? {
                    table.insert(key, value);
                }
            }
            table.into()
        }
    }))
}

/// Replace the file at `path` with `text`: the old content goes to `<path>.bak`, the new is
/// written beside it with the same permissions, synced, and renamed over it.
pub fn replace(path: &Path, text: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("configuration directory")?;
    std::fs::copy(path, path.with_extension("toml.bak"))?;
    let mut next = tempfile::NamedTempFile::new_in(dir)?;
    next.as_file()
        .set_permissions(std::fs::metadata(path)?.permissions())?;
    next.write_all(text.as_bytes())?;
    next.as_file().sync_all()?;
    next.persist(path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Whether the file at `path` can be replaced: it and its directory are writable.
pub fn writable(path: &Path) -> bool {
    use nix::unistd::{AccessFlags, access};
    path.parent().is_some_and(|dir| {
        access(path, AccessFlags::W_OK).is_ok() && access(dir, AccessFlags::W_OK).is_ok()
    })
}

/// Hold the configuration directory's edit lock while `f` runs.
pub fn locked<T>(path: &Path, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let name = path.file_name().context("configuration file")?;
    let lock = path.with_file_name(format!(".{}.lock", name.to_string_lossy()));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock)?;
    lock.lock()?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(text: &str, values: Value) -> String {
        let values: BTreeMap<String, Value> = serde_json::from_value(values).unwrap();
        apply(text, &values).unwrap()
    }

    #[test]
    fn an_edit_keeps_comments_order_and_everything_else() {
        let text = "# Instance.\nlisten = \"0.0.0.0:8080\" # all interfaces\n\n[llm]\n# The default.\nmodel = \"a\" # cheap\nmax_tokens = 10\n\n[[llm.models]]\nid = \"old\"\n\n[policy]\nmax_sandboxes = 3\n";
        let edited = set(
            text,
            json!({
                "llm.model": "b",
                "llm.models": [{"id": "new", "protocol": "responses",
                    "upstreams": [{"url": "https://u.test", "weight": 2}]}],
                "policy.max_sandboxes": null,
                "approval_ttl_secs": 60,
                "plugins.trusted_keys": {"k": "v"},
            }),
        );
        assert!(edited.starts_with("# Instance.\nlisten = \"0.0.0.0:8080\" # all interfaces\n"));
        assert!(
            edited.contains("# The default.\nmodel = \"b\" # cheap\nmax_tokens = 10\n"),
            "{edited}"
        );
        assert!(!edited.contains("max_sandboxes") && !edited.contains("old"));
        let config: Config = super::super::parse_toml(&edited).unwrap();
        assert_eq!(config.llm.models[0].upstreams[0].weight, 2);
        assert_eq!(config.approval_ttl_secs, 60);
        assert_eq!(config.plugins.trusted_keys["k"], "v");
        assert_eq!(config.policy.max_sandboxes, None);
    }

    #[test]
    fn model_keys_come_only_from_variables_already_used_for_model_keys() {
        let mut config = Config::default();
        config.llm.models.push(crate::config::ModelRoute {
            upstreams: vec![crate::config::Upstream {
                key_env: "DEEPSEEK_API_KEY".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        // Even named for a model, the vault key is never one.
        config.search.embedding_key_env = "RYZHUB_VAULT_KEY".into();
        let allowed = key_envs(&config);
        assert!(allowed.contains("DEEPSEEK_API_KEY") && allowed.contains("OPENAI_API_KEY"));
        assert!(!allowed.contains("RYZHUB_VAULT_KEY"));
        let route = |env: &str| json!([{"id": "m", "upstreams": [{"key_env": env}]}]);
        assert!(names_allowed_env(
            "llm.models",
            &route("DEEPSEEK_API_KEY"),
            &allowed
        ));
        assert!(names_allowed_env("llm.models", &route(""), &allowed));
        assert!(!names_allowed_env(
            "llm.models",
            &route("RYZHUB_VAULT_KEY"),
            &allowed
        ));
        assert!(!names_allowed_env(
            "llm.platform_key_env",
            &json!("PATH"),
            &allowed
        ));
        assert!(!names_allowed_env(
            "llm.platform_key_env",
            &json!(""),
            &allowed
        ));
        assert!(names_allowed_env(
            "search.embedding_key_env",
            &json!(""),
            &allowed
        ));
    }
}
