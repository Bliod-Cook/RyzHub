//! A plugin server's questions (MCP elicitation). On the stateless protocol a tool call answers
//! `input_required` with the requests; the platform asks the member and calls again with the
//! responses. A form's answer is a JSON object of its fields; a page's is `accept` or `decline`.
use anyhow::Context;
use serde_json::{Map, Value, json};

use crate::error::Code;

/// The question to ask for `requests`: the key of the elicitation and its form. None when the
/// server asks for something else (sampling, roots), which the platform does not provide.
pub fn question(requests: &Value) -> Option<(String, Value)> {
    let requests = requests.as_object()?;
    if requests.is_empty()
        || requests
            .values()
            .any(|r| r["method"] != "elicitation/create")
    {
        return None;
    }
    let (key, request) = requests.iter().next()?;
    let params = &request["params"];
    let mode = match params["mode"].as_str() {
        Some(mode) => mode,
        None if params["url"].is_string() => "url",
        None => "form",
    };
    let form = match mode {
        "url" => json!({
            "mode": "url",
            "message": params["message"],
            "url": params["url"].as_str()?,
        }),
        _ => json!({
            "mode": "form",
            "message": params["message"],
            "requestedSchema": params["requestedSchema"],
        }),
    };
    Some((key.clone(), form))
}

/// The responses to send back: the member's `answer` to request `key`; any other request of
/// the same round is declined.
pub fn responses(requests: &Value, key: &str, answer: &str) -> anyhow::Result<Value> {
    let mut responses = Map::new();
    for name in requests.as_object().context(Code::InvalidArguments)?.keys() {
        let response = if name != key {
            json!({"action": "decline"})
        } else {
            match answer {
                "decline" | "cancel" => json!({"action": answer}),
                "accept" => json!({"action": "accept"}),
                text => {
                    let content: Value =
                        serde_json::from_str(text).map_err(|_| Code::InvalidArguments)?;
                    anyhow::ensure!(content.is_object(), Code::InvalidArguments);
                    json!({"action": "accept", "content": content})
                }
            }
        };
        responses.insert(name.clone(), response);
    }
    Ok(Value::Object(responses))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_is_asked_and_answered() {
        let requests = json!({"ask": {"method": "elicitation/create", "params": {
            "message": "Which project?",
            "requestedSchema": {"type": "object", "properties": {"project": {"type": "string"}}},
        }}});
        let (key, form) = question(&requests).unwrap();
        assert_eq!((key.as_str(), form["mode"].as_str()), ("ask", Some("form")));
        assert_eq!(
            responses(&requests, "ask", r#"{"project":"alpha"}"#).unwrap(),
            json!({"ask": {"action": "accept", "content": {"project": "alpha"}}})
        );
        assert_eq!(
            responses(&requests, "ask", "decline").unwrap(),
            json!({"ask": {"action": "decline"}})
        );
        assert!(responses(&requests, "ask", "not json").is_err());
        let page = json!({"login": {"method": "elicitation/create", "params": {"mode": "url", "message": "Sign in", "url": "https://example.com/auth"}}});
        assert_eq!(
            question(&page).unwrap().1["url"],
            "https://example.com/auth"
        );
        // Sampling is not something the platform does for plugins.
        assert!(
            question(&json!({"s": {"method": "sampling/createMessage", "params": {}}})).is_none()
        );
    }
}
