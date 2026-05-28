//! User-defined bash-command tools (`webfetch`, `websearch`, …).
//!
//! Built from a [`crate::config::extended::ToolCommandTemplate`]: the
//! `command` field is a shell template with `{placeholder}` markers; each
//! distinct placeholder becomes a string parameter the model must supply.
//! At call time we substitute the args back in (shell-escaped) and run
//! the result through `/bin/sh -c`.
//!
//! Token economy (CLAUDE.md): the description string is whatever the
//! user typed in `extended-config.tools.<name>.description`. If they
//! left it blank we synthesize a one-liner from the tool name + the
//! placeholder list.

use std::collections::BTreeSet;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::config::extended::ToolCommandTemplate;
use crate::engine::tool::{Tool, ToolCtx, ToolOutput};
use crate::tools::common::{OUTPUT_BYTE_CAP, truncate_head_tail};

const SHELL_TIMEOUT_SECS: u64 = 30;

pub struct CustomBashTool {
    name: String,
    description: String,
    template: String,
    /// Stable-ordered list of placeholder names the template uses.
    params: Vec<String>,
}

impl CustomBashTool {
    pub fn from_template(name: &str, tpl: &ToolCommandTemplate) -> Self {
        let params = extract_placeholders(&tpl.command);
        let description = tpl
            .description
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| synth_description(name, &params));
        Self {
            name: name.to_string(),
            description,
            template: tpl.command.clone(),
            params,
        }
    }

    fn build_schema(&self) -> Value {
        let mut props = serde_json::Map::new();
        for p in &self.params {
            props.insert(
                p.clone(),
                serde_json::json!({
                    "type": "string",
                    "description": format!("Value substituted for `{{{p}}}` in the bash template.")
                }),
            );
        }
        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": self.params.clone(),
        })
    }
}

#[async_trait]
impl Tool for CustomBashTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.build_schema()
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let mut cmd = self.template.clone();
        for p in &self.params {
            let raw = args.get(p).and_then(Value::as_str).unwrap_or("");
            let quoted = shell_quote(raw);
            cmd = cmd.replace(&format!("{{{p}}}"), &quoted);
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(SHELL_TIMEOUT_SECS),
            tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&cmd)
                .output(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("tool `{}` timed out after {SHELL_TIMEOUT_SECS}s", self.name)
        })??;

        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        if !output.status.success() {
            combined.push_str("\n[stderr]\n");
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        if combined.len() > OUTPUT_BYTE_CAP {
            // Byte-boundary-safe; `String::truncate` would panic on a
            // multibyte boundary. Head+tail keeps any appended stderr.
            return Ok(ToolOutput::truncated_text(truncate_head_tail(
                &combined,
                OUTPUT_BYTE_CAP,
            )));
        }
        Ok(ToolOutput::text(combined))
    }
}

/// Pull every `{placeholder}` token from the template. Order = first
/// appearance, deduplicated.
fn extract_placeholders(template: &str) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = template[i + 1..].find('}') {
                let name = &template[i + 1..i + 1 + end];
                if is_ident(name) && !seen.contains(name) {
                    seen.insert(name.to_string());
                    out.push(name.to_string());
                }
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// POSIX single-quote escape. The model-supplied value lands inside the
/// template verbatim — no shell expansion, no env interpolation.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | ':' | '.' | '@' | '+' | '%')
    }) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn synth_description(name: &str, params: &[String]) -> String {
    if params.is_empty() {
        format!("Run the configured `{name}` command.")
    } else {
        let plist = params
            .iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Run the configured `{name}` command. Args: {plist}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_extraction_finds_named_tokens_once() {
        let tpl = "curl -sSL --max-time {timeout} {url} | head -c {bytes} # ignore {timeout}";
        let p = extract_placeholders(tpl);
        assert_eq!(
            p,
            vec![
                "timeout".to_string(),
                "url".to_string(),
                "bytes".to_string()
            ]
        );
    }

    #[test]
    fn placeholder_extraction_skips_non_ident() {
        // `{ }` and `{a b}` aren't valid placeholders; we leave them as
        // literal command text.
        let tpl = "echo {a b} {valid} {}";
        let p = extract_placeholders(tpl);
        assert_eq!(p, vec!["valid".to_string()]);
    }

    #[test]
    fn shell_quote_passes_through_safe_chars() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("path/to-file.rs"), "path/to-file.rs");
        assert_eq!(shell_quote("user@host"), "user@host");
    }

    #[test]
    fn shell_quote_wraps_dangerous_chars() {
        assert_eq!(shell_quote("hi there"), "'hi there'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("it's a trap"), "'it'\\''s a trap'");
    }

    #[test]
    fn schema_has_required_string_params() {
        let tpl = ToolCommandTemplate {
            enabled: true,
            command: "echo {who}".into(),
            description: None,
        };
        let tool = CustomBashTool::from_template("greet", &tpl);
        let schema = tool.build_schema();
        assert_eq!(schema["required"], serde_json::json!(["who"]));
        assert_eq!(schema["properties"]["who"]["type"], "string");
    }
}
