//! Docs-pipeline-only tools: `list-packages` and `add-package`
//! (prompt `docs-agent.md` component C, Docs.1 resolver surface).
//!
//! These are assigned exclusively to the Docs.1 *resolver* stage. Their
//! job is to get a dependency's source into cockpit's package registry
//! (cloning it shallowly from registry-declared metadata if absent —
//! decision 4) so the pipeline can launch Docs.2 in the resolved package
//! directory.
//!
//! Resolution side-channel: both tools record the resolved on-disk path
//! into a shared [`DocsResolution`] the pipeline owns. The pipeline reads
//! it after Docs.1 finishes to decide whether to launch Docs.2 — this is
//! deterministic, not a parse of the model's free text (priority #1:
//! defensive against weak models).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::db::packages::SourceType;
use crate::engine::tool::{Tool, ToolCtx, ToolOutput, invalid_input};
use crate::packages::resolve::{RepoResolution, resolve_repo_url};
use crate::packages::{self, Ecosystem};

/// Shared resolution slot threaded into the Docs.1 tools and read back by
/// the pipeline. Records the on-disk path of the package the resolver
/// confirmed/cloned, plus its identifier for the citation header.
#[derive(Default)]
pub struct DocsResolution {
    inner: Mutex<Option<Resolved>>,
}

#[derive(Clone)]
pub struct Resolved {
    pub identifier: String,
    pub path: std::path::PathBuf,
}

impl DocsResolution {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record(&self, identifier: &str, path: &std::path::Path) {
        *self.inner.lock().expect("docs resolution mutex") = Some(Resolved {
            identifier: identifier.to_string(),
            path: path.to_path_buf(),
        });
    }

    /// The resolved package, if Docs.1 located one with a path that
    /// still exists on disk (an imported kcl clone may have been
    /// removed — tolerate that cleanly per the edge-case spec).
    pub fn take(&self) -> Option<Resolved> {
        let resolved = self.inner.lock().expect("docs resolution mutex").clone();
        resolved.filter(|r| r.path.is_dir())
    }
}

/// `list-packages` — list every registered package so the resolver can
/// see whether the dependency it needs is already present.
pub struct ListPackagesTool {
    resolution: Arc<DocsResolution>,
    /// The package the pipeline asked Docs.1 to resolve. Listing a match
    /// for it records the resolution side-channel.
    target: String,
}

impl ListPackagesTool {
    pub fn new(resolution: Arc<DocsResolution>, target: String) -> Self {
        Self { resolution, target }
    }
}

#[async_trait]
impl Tool for ListPackagesTool {
    fn name(&self) -> &str {
        "list-packages"
    }

    fn description(&self) -> &str {
        "List registered dependency packages available to the docs answerer"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let packages = ctx.session.db.list_packages()?;
        if packages.is_empty() {
            return Ok(ToolOutput::text(
                "No packages registered. Use add-package to clone the dependency's source."
                    .to_string(),
            ));
        }
        let mut out = String::new();
        for p in &packages {
            // If a registered package matches the requested target,
            // record it as resolved so the pipeline can proceed without
            // a clone.
            if identifier_matches(&p.identifier, &self.target) {
                self.resolution
                    .record(&p.identifier, std::path::Path::new(&p.path));
            }
            out.push_str(&format!("{}  [{}]\n", p.identifier, p.source_type.as_str()));
        }
        Ok(ToolOutput::text(out))
    }
}

/// `add-package` — register a dependency by cloning its source from a
/// registry-declared repo (decision 4: never a guessed URL). Resolves
/// the repo from crates.io / npm / PyPI metadata for the named
/// ecosystem; refuses to clone when no source repo is declared.
pub struct AddPackageTool {
    resolution: Arc<DocsResolution>,
}

impl AddPackageTool {
    pub fn new(resolution: Arc<DocsResolution>) -> Self {
        Self { resolution }
    }
}

#[async_trait]
impl Tool for AddPackageTool {
    fn name(&self) -> &str {
        "add-package"
    }

    fn description(&self) -> &str {
        "Clone a dependency's source from its official registry-declared repo and register it"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name":      { "type": "string", "description": "Package name as published (e.g. `tokio`, `requests`)" },
                "ecosystem": { "type": "string", "description": "Registry to resolve the source repo from", "enum": ["cargo", "npm", "pip"] }
            },
            "required": ["name", "ecosystem"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_input("`name` is required"))?;
        let eco_str = args
            .get("ecosystem")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("`ecosystem` is required (cargo|npm|pip)"))?;
        let eco = Ecosystem::parse(eco_str)
            .ok_or_else(|| invalid_input(format!("unknown ecosystem `{eco_str}`")))?;

        let identifier = packages::ecosystem_slug(eco, name);

        // Already registered under the ecosystem-prefixed identifier?
        if let Some(existing) = ctx.session.db.package_by_identifier(&identifier)? {
            self.resolution
                .record(&existing.identifier, std::path::Path::new(&existing.path));
            return Ok(ToolOutput::text(format!(
                "`{identifier}` is already registered at {}.",
                existing.path
            )));
        }

        // Resolve the source repo from official registry metadata only.
        let repo = match resolve_repo_url(eco, name).await {
            Ok(RepoResolution::Resolved(url)) => url,
            Ok(RepoResolution::NotDeclared) => {
                return Ok(ToolOutput::text(format!(
                    "Could not resolve a source repo for `{name}`: the {eco_str} registry declares no repository. Refusing to clone a guessed URL."
                )));
            }
            Err(e) => {
                return Ok(ToolOutput::text(format!(
                    "Could not look up `{name}` on the {eco_str} registry: {e}"
                )));
            }
        };

        let row = match packages::add_git(&ctx.session.db, &ctx.cwd, &identifier, &repo, None, true)
        {
            Ok(row) => row,
            Err(e) => {
                return Ok(ToolOutput::text(format!(
                    "Could not clone `{name}` from {repo}: {e}"
                )));
            }
        };
        debug_assert_eq!(row.source_type, SourceType::Git);
        self.resolution
            .record(&row.identifier, std::path::Path::new(&row.path));
        Ok(ToolOutput::text(format!(
            "Registered `{identifier}` from {repo} at {}.",
            row.path
        )))
    }
}

/// Whether a registered `identifier` satisfies a requested `target`. The
/// caller's `package` is a bare name (`tokio`) or scoped name
/// (`@tanstack/query`); registered identifiers may be bare (kcl imports)
/// or ecosystem-prefixed (`cargo:tokio`). Match either form.
fn identifier_matches(identifier: &str, target: &str) -> bool {
    if identifier == target {
        return true;
    }
    // Strip an ecosystem prefix (`cargo:`, `npm:`, `pip:`) and compare.
    identifier
        .split_once(':')
        .is_some_and(|(_, rest)| rest == target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::common::test_ctx;

    #[test]
    fn identifier_matching() {
        assert!(identifier_matches("tokio", "tokio"));
        assert!(identifier_matches("cargo:tokio", "tokio"));
        assert!(identifier_matches("npm:@tanstack/query", "@tanstack/query"));
        assert!(!identifier_matches("cargo:tokio", "serde"));
    }

    #[tokio::test]
    async fn list_packages_records_matching_target() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        // Register a package whose on-disk path exists.
        let pkg_dir = tmp.path().join("clone");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        ctx.session
            .db
            .upsert_package(&crate::db::packages::NewPackage {
                identifier: "cargo:tokio".into(),
                display_name: "tokio".into(),
                source_type: SourceType::Git,
                source_url: Some("u".into()),
                source_branch: Some("main".into()),
                path: pkg_dir.to_string_lossy().into(),
                shallow: true,
            })
            .unwrap();
        let resolution = DocsResolution::new();
        let tool = ListPackagesTool::new(resolution.clone(), "tokio".into());
        let _ = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        let resolved = resolution.take().expect("expected a resolution");
        assert_eq!(resolved.identifier, "cargo:tokio");
        assert_eq!(resolved.path, pkg_dir);
    }

    #[tokio::test]
    async fn list_packages_empty_message() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        let resolution = DocsResolution::new();
        let tool = ListPackagesTool::new(resolution.clone(), "tokio".into());
        let out = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("No packages registered"));
        assert!(resolution.take().is_none());
    }
}
