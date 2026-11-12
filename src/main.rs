//! CalDAV MCP server (stdio transport, JSON-RPC 2.0, one message per line).
//!
//! Configuration (environment variables):
//!   CALDAV_URL               server, principal or calendar-home URL (required)
//!   CALDAV_USERNAME          username for HTTP Basic auth
//!   CALDAV_PASSWORD          password / app password for HTTP Basic auth
//!   CALDAV_TOKEN             bearer token (alternative to username/password)
//!   CALDAV_DEFAULT_CALENDAR  default calendar name or URL
//!   CALDAV_DEFAULT_TASK_LIST default calendar for tasks (VTODO)
//!   CALDAV_TIMEZONE          default IANA timezone for local date-times
//!   NEXTCLOUD_URL            Nextcloud server root; enables the Nextcloud Deck tools (optional)
//!   NEXTCLOUD_USERNAME / NEXTCLOUD_PASSWORD / NEXTCLOUD_TOKEN
//!                            Deck credentials (default: the CALDAV_* credentials)

mod caldav;
mod deck;
mod deck_tools;
mod expand;
mod ical;
mod tools;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use caldav::{Auth, CalDavClient, Config};
use deck::DeckClient;
use tools::Tools;

const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn build_tools() -> Result<Tools> {
    let url = env("CALDAV_URL").context("CALDAV_URL environment variable is required")?;
    let base_url = url::Url::parse(&url).with_context(|| format!("invalid CALDAV_URL '{url}'"))?;
    let auth = match (env("CALDAV_TOKEN"), env("CALDAV_USERNAME")) {
        (Some(t), _) => Auth::Bearer(t),
        (None, Some(user)) => Auth::Basic { user, pass: env("CALDAV_PASSWORD").unwrap_or_default() },
        (None, None) => Auth::None,
    };
    if let Some(tz) = env("CALDAV_TIMEZONE") {
        ical::parse_tz(&tz).ok_or_else(|| anyhow::anyhow!("CALDAV_TIMEZONE '{tz}' is not a valid IANA timezone"))?;
    }
    let deck = match env("NEXTCLOUD_URL") {
        Some(nc) => {
            let server = url::Url::parse(&nc).with_context(|| format!("invalid NEXTCLOUD_URL '{nc}'"))?;
            let deck_auth = match (env("NEXTCLOUD_TOKEN"), env("NEXTCLOUD_USERNAME")) {
                (Some(t), _) => Auth::Bearer(t),
                (None, Some(user)) => Auth::Basic { user, pass: env("NEXTCLOUD_PASSWORD").unwrap_or_default() },
                (None, None) => auth.clone(),
            };
            if matches!(deck_auth, Auth::None) {
                anyhow::bail!("NEXTCLOUD_URL is set but no credentials are configured (NEXTCLOUD_USERNAME/NEXTCLOUD_PASSWORD, NEXTCLOUD_TOKEN or the CALDAV_* ones)");
            }
            Some(DeckClient::new(server, deck_auth)?)
        }
        None => None,
    };
    Ok(Tools {
        client: CalDavClient::new(Config { base_url, auth })?,
        default_calendar: env("CALDAV_DEFAULT_CALENDAR"),
        default_task_list: env("CALDAV_DEFAULT_TASK_LIST"),
        default_timezone: env("CALDAV_TIMEZONE"),
        deck,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let tools = match build_tools() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("caldav_mcp: configuration error: {e:#}");
            std::process::exit(1);
        }
    };

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let mut out = Vec::new();
                for msg in batch {
                    if let Some(r) = handle(&tools, msg).await {
                        out.push(r);
                    }
                }
                (!out.is_empty()).then_some(Value::Array(out))
            }
            Ok(msg) => handle(&tools, msg).await,
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(r) = response {
            let mut s = serde_json::to_string(&r)?;
            s.push('\n');
            stdout.write_all(s.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

fn instructions(deck: bool) -> String {
    let mut s = String::from(
        "Tools for reading and writing calendar events and tasks (to-dos) over CalDAV. Call list_calendars first to discover calendars (components shows whether a calendar holds events, VEVENT, and/or tasks, VTODO). list_events returns event_url values that get_event/update_event/delete_event accept; list_tasks returns task_url values that get_task/update_task/delete_task accept.",
    );
    if deck {
        s.push_str(" The deck_* tools work on Nextcloud Deck kanban boards: deck_list_boards shows boards with their labels and users, deck_list_stacks shows a board's stacks (columns) with their cards, and the card tools take board + card_id from there.");
    }
    s
}

fn error(id: Value, code: i64, msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

async fn handle(tools: &Tools, msg: Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = msg.get("id").cloned() else {
        // Notification (e.g. notifications/initialized): no response.
        return None;
    };
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if SUPPORTED_PROTOCOLS.contains(&requested) { requested } else { SUPPORTED_PROTOCOLS[0] };
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "caldav_mcp", "version": env!("CARGO_PKG_VERSION") },
                "instructions": instructions(tools.deck.is_some())
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::definitions(tools.deck.is_some()) }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match tools.call(name, &args).await {
                Ok(v) => json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                    "structuredContent": v,
                    "isError": false
                }),
                Err(e) => json!({
                    "content": [{ "type": "text", "text": format!("Error: {e:#}") }],
                    "isError": true
                }),
            }
        }
        "resources/list" => json!({ "resources": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}
