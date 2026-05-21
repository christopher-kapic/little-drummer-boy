use anyhow::Result;

/// `cockpit mcp …` is a no-op pointer — MCP support is intentionally out of
/// scope (see `GOALS.md` non-goals). Users should install `mcp2cli-rs` and
/// invoke MCP tools via the regular `bash` tool, which is dramatically
/// cheaper in tokens.
pub async fn run() -> Result<()> {
    eprintln!(
        "cockpit does not implement MCP. Install mcp2cli to expose MCP servers \
         as CLI commands the model can invoke through its `bash` tool:\n\
         \n  cargo install mcp2cli\n\n\
         See https://github.com/christopher-kapic/mcp2cli-rs"
    );
    std::process::exit(1);
}
