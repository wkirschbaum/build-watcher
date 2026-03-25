use std::path::PathBuf;

use anyhow::{Context, Result};

/// Register build-watcher as an MCP server in Claude Code config files.
///
/// - Adds the MCP server entry to `~/.claude.json`
/// - Adds `mcp__build-watcher__*` to the allow list in `~/.claude/settings.json`
pub fn register(port: u16) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;

    register_mcp_server(&home, port)?;
    register_permissions(&home)?;

    println!("Registered build-watcher MCP server (port {port})");
    Ok(())
}

fn register_mcp_server(home: &str, port: u16) -> Result<()> {
    let path = PathBuf::from(home).join(".claude.json");

    let mut config: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?
    } else {
        serde_json::Map::new()
    };

    let servers = config
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    if let Some(obj) = servers.as_object_mut() {
        let entry = serde_json::json!({
            "type": "http",
            "url": format!("http://127.0.0.1:{port}/mcp")
        });
        obj.insert("build-watcher".to_string(), entry);
    }

    write_json(&path, &config)?;
    println!("  MCP config: {}", path.display());
    Ok(())
}

fn register_permissions(home: &str) -> Result<()> {
    let path = PathBuf::from(home).join(".claude/settings.json");
    if !path.exists() {
        return Ok(());
    }

    let data =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut settings: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))?;

    let perms = settings
        .entry("permissions")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    let entry = "mcp__build-watcher__*";

    if let Some(perms_obj) = perms.as_object_mut() {
        let allow = perms_obj
            .entry("allow")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));

        if let Some(arr) = allow.as_array_mut()
            && !arr.iter().any(|v| v.as_str() == Some(entry))
        {
            arr.push(serde_json::Value::String(entry.to_string()));
            write_json(&path, &settings)?;
            println!("  Permissions: {}", path.display());
        }
    }

    Ok(())
}

fn write_json<T: serde::Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    let data = serde_json::to_string_pretty(value)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{data}\n"))?;
    Ok(())
}
