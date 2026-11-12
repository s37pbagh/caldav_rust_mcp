//! Nextcloud Deck REST API client (https://deck.readthedocs.io/en/latest/API/).
//!
//! Requests are built by pure functions (`req::*`) and responses are turned into tool output by
//! pure functions (`*_json`), so both can be tested without a server.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use reqwest::{Method, StatusCode, header};
use serde_json::{Map, Value, json};
use url::Url;

use crate::caldav::Auth;
use crate::ical::{self, DtInput};

/// API root relative to the Nextcloud server root.
const API_PATH: &str = "index.php/apps/deck/api/v1.0/";

pub struct DeckClient {
    http: reqwest::Client,
    api: Url,
    auth: Auth,
}

/// One Deck API call: method, path relative to the API root and optional JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiRequest {
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
}

/// Request builders, one per Deck API endpoint used by the tools.
pub mod req {
    use super::*;

    fn get(path: String) -> ApiRequest {
        ApiRequest { method: Method::GET, path, body: None }
    }

    fn card_path(board: i64, stack: i64, card: i64) -> String {
        format!("boards/{board}/stacks/{stack}/cards/{card}")
    }

    pub fn boards() -> ApiRequest {
        get("boards?details=true".into())
    }

    pub fn board(board: i64) -> ApiRequest {
        get(format!("boards/{board}"))
    }

    pub fn stacks(board: i64, archived: bool) -> ApiRequest {
        get(format!("boards/{board}/stacks{}", if archived { "/archived" } else { "" }))
    }

    pub fn card(board: i64, stack: i64, card: i64) -> ApiRequest {
        get(card_path(board, stack, card))
    }

    pub fn create_card(board: i64, stack: i64, title: &str, description: &str, duedate: Option<&str>) -> ApiRequest {
        ApiRequest {
            method: Method::POST,
            path: format!("boards/{board}/stacks/{stack}/cards"),
            body: Some(json!({ "title": title, "type": "plain", "order": 999, "description": description, "duedate": duedate })),
        }
    }

    /// Full card update. The v1.0 endpoint resets fields that are left out, so every field is sent.
    pub fn update_card(board: i64, stack: i64, card: i64, fields: &CardFields) -> ApiRequest {
        ApiRequest {
            method: Method::PUT,
            path: card_path(board, stack, card),
            body: Some(json!({
                "title": fields.title,
                "type": fields.kind,
                "owner": fields.owner,
                "description": fields.description,
                "order": fields.order,
                "duedate": fields.duedate,
                "archived": fields.archived,
            })),
        }
    }

    /// Move a card to `to_stack` at position `order` (0 = top).
    /// The path names the target stack: Nextcloud lets the stack id in the URL override the one in
    /// the body, so a path with the current stack would only reorder within it.
    pub fn reorder(board: i64, card: i64, to_stack: i64, order: i64) -> ApiRequest {
        ApiRequest {
            method: Method::PUT,
            path: format!("{}/reorder", card_path(board, to_stack, card)),
            body: Some(json!({ "order": order, "stackId": to_stack })),
        }
    }

    pub fn label(board: i64, stack: i64, card: i64, label: i64, assign: bool) -> ApiRequest {
        let action = if assign { "assignLabel" } else { "removeLabel" };
        ApiRequest {
            method: Method::PUT,
            path: format!("{}/{action}", card_path(board, stack, card)),
            body: Some(json!({ "labelId": label })),
        }
    }

    pub fn user(board: i64, stack: i64, card: i64, user: &str, assign: bool) -> ApiRequest {
        let action = if assign { "assignUser" } else { "unassignUser" };
        ApiRequest {
            method: Method::PUT,
            path: format!("{}/{action}", card_path(board, stack, card)),
            body: Some(json!({ "userId": user })),
        }
    }

    pub fn delete_card(board: i64, stack: i64, card: i64) -> ApiRequest {
        ApiRequest { method: Method::DELETE, path: card_path(board, stack, card), body: None }
    }
}

/// The writable fields of a card, as sent by a full update.
#[derive(Debug, Clone, PartialEq)]
pub struct CardFields {
    pub title: String,
    pub kind: String,
    pub owner: String,
    pub description: String,
    pub order: i64,
    pub duedate: Option<String>,
    pub archived: bool,
}

impl CardFields {
    /// Current values of a card as returned by the API.
    pub fn from_card(card: &Value) -> Result<Self> {
        let owner = user_id(card.get("owner").unwrap_or(&Value::Null)).ok_or_else(|| anyhow!("card has no owner"))?;
        Ok(Self {
            title: str_field(card, "title").unwrap_or_default(),
            kind: str_field(card, "type").unwrap_or_else(|| "plain".into()),
            owner,
            description: str_field(card, "description").unwrap_or_default(),
            order: card.get("order").and_then(Value::as_i64).unwrap_or(0),
            duedate: str_field(card, "duedate").filter(|d| !d.is_empty()),
            archived: card.get("archived").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

impl DeckClient {
    /// `server` is the Nextcloud root, e.g. https://cloud.example.com or https://example.com/nextcloud.
    pub fn new(server: Url, auth: Auth) -> Result<Self> {
        let mut root = server;
        if !root.path().ends_with('/') {
            let p = format!("{}/", root.path());
            root.set_path(&p);
        }
        let api = root.join(API_PATH)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("caldav_mcp/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self { http, api, auth })
    }

    pub fn url(&self, path: &str) -> Result<Url> {
        self.api.join(path).with_context(|| format!("invalid Deck API path '{path}'"))
    }

    pub async fn send(&self, r: ApiRequest) -> Result<Value> {
        let url = self.url(&r.path)?;
        let mut req = self
            .http
            .request(r.method.clone(), url.clone())
            .header("OCS-APIRequest", "true")
            .header(header::ACCEPT, "application/json");
        req = match &self.auth {
            Auth::None => req,
            Auth::Basic { user, pass } => req.basic_auth(user, Some(pass)),
            Auth::Bearer(t) => req.bearer_auth(t),
        };
        if let Some(b) = &r.body {
            req = req.header(header::CONTENT_TYPE, "application/json").body(serde_json::to_string(b)?);
        }
        let resp = req.send().await.with_context(|| format!("{} {url} failed", r.method))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status, &body, &r));
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).with_context(|| format!("Deck API {} {} returned invalid JSON", r.method, r.path))
    }
}

/// Turn a failed API response into an error that says what went wrong and what to check.
pub fn api_error(status: StatusCode, body: &str, r: &ApiRequest) -> anyhow::Error {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        v.get("message")
            .or_else(|| v.pointer("/ocs/meta/message"))
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
    });
    let detail = message.map(|m| format!(": {m}")).unwrap_or_default();
    let what = format!("{} {}", r.method, r.path);
    match status {
        StatusCode::UNAUTHORIZED => anyhow!(
            "Nextcloud rejected the credentials (401) for {what}. Check NEXTCLOUD_USERNAME / NEXTCLOUD_PASSWORD (or the CALDAV_* ones); with two-factor authentication use an app password"
        ),
        StatusCode::FORBIDDEN => anyhow!("permission denied (403) for {what}{detail}"),
        StatusCode::NOT_FOUND if parsed.is_none() => anyhow!(
            "Deck API not found (404) at {what}. Is the Deck app installed and enabled, and is NEXTCLOUD_URL the Nextcloud server root (e.g. https://cloud.example.com)?"
        ),
        StatusCode::NOT_FOUND => anyhow!("not found (404): {what}{detail}"),
        StatusCode::BAD_REQUEST => anyhow!("Deck rejected the request (400) {what}{detail}"),
        _ => anyhow!("Deck API {what} returned {status}{}", if detail.is_empty() { truncate_detail(body) } else { detail }),
    }
}

fn truncate_detail(body: &str) -> String {
    let b = body.trim();
    if b.is_empty() {
        return String::new();
    }
    let cut: String = b.chars().take(300).collect();
    format!(": {cut}")
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

/// User id from a participant / owner value (an object with `uid` or `primaryKey`, or a plain string).
pub fn user_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("uid").or_else(|| o.get("primaryKey")).and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

pub fn user_json(v: &Value) -> Value {
    let mut m = Map::new();
    if let Some(id) = user_id(v) {
        m.insert("id".into(), json!(id));
    }
    if let Some(name) = v.get("displayname").and_then(Value::as_str) {
        m.insert("name".into(), json!(name));
    }
    Value::Object(m)
}

pub fn label_json(l: &Value) -> Value {
    json!({ "id": l.get("id"), "title": l.get("title"), "color": l.get("color") })
}

fn is_deleted(v: &Value) -> bool {
    v.get("deletedAt").and_then(Value::as_i64).is_some_and(|d| d > 0)
}

pub fn board_json(b: &Value) -> Value {
    let mut m = Map::new();
    m.insert("board_id".into(), b.get("id").cloned().unwrap_or(Value::Null));
    m.insert("title".into(), b.get("title").cloned().unwrap_or(Value::Null));
    if let Some(c) = b.get("color").and_then(Value::as_str) {
        m.insert("color".into(), json!(c));
    }
    m.insert("archived".into(), json!(b.get("archived").and_then(Value::as_bool).unwrap_or(false)));
    if let Some(owner) = b.get("owner").and_then(user_id) {
        m.insert("owner".into(), json!(owner));
    }
    if let Some(labels) = b.get("labels").and_then(Value::as_array) {
        m.insert("labels".into(), labels.iter().map(label_json).collect());
    }
    if let Some(users) = b.get("users").and_then(Value::as_array) {
        m.insert("users".into(), users.iter().map(user_json).collect());
    }
    Value::Object(m)
}

/// Boards that are not deleted (and not archived unless `include_archived`).
pub fn visible_boards(boards: &Value, include_archived: bool) -> Vec<&Value> {
    boards
        .as_array()
        .map(|a| a.iter().filter(|b| !is_deleted(b) && (include_archived || !b.get("archived").and_then(Value::as_bool).unwrap_or(false))).collect())
        .unwrap_or_default()
}

/// Render a Deck ISO-8601 timestamp in the display timezone.
pub fn render_due(due: &str, display: Option<Tz>) -> String {
    match DateTime::parse_from_rfc3339(due).or_else(|_| DateTime::parse_from_str(due, "%Y-%m-%dT%H:%M:%S%z")) {
        Ok(dt) => match display {
            Some(tz) => dt.with_timezone(&tz).format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            None => dt.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        },
        Err(_) => due.to_string(),
    }
}

fn render_unix(secs: i64, display: Option<Tz>) -> Option<String> {
    let dt = DateTime::from_timestamp(secs, 0)?;
    Some(render_due(&dt.to_rfc3339(), display))
}

pub fn card_json(c: &Value, display: Option<Tz>) -> Value {
    let mut m = Map::new();
    m.insert("card_id".into(), c.get("id").cloned().unwrap_or(Value::Null));
    m.insert("stack_id".into(), c.get("stackId").cloned().unwrap_or(Value::Null));
    m.insert("title".into(), c.get("title").cloned().unwrap_or(Value::Null));
    if let Some(d) = c.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        m.insert("description".into(), json!(d));
    }
    if let Some(d) = c.get("duedate").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        m.insert("due".into(), json!(render_due(d, display)));
    }
    if let Some(d) = c.get("done").and_then(Value::as_str).filter(|d| !d.is_empty()) {
        m.insert("done".into(), json!(render_due(d, display)));
    }
    let labels: Vec<Value> = c.get("labels").and_then(Value::as_array).map(|a| a.iter().map(label_json).collect()).unwrap_or_default();
    m.insert("labels".into(), json!(labels));
    let users: Vec<Value> = c
        .get("assignedUsers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|u| user_json(u.get("participant").unwrap_or(u))).collect())
        .unwrap_or_default();
    m.insert("assigned_users".into(), json!(users));
    if let Some(owner) = c.get("owner").and_then(user_id) {
        m.insert("owner".into(), json!(owner));
    }
    m.insert("archived".into(), json!(c.get("archived").and_then(Value::as_bool).unwrap_or(false)));
    if let Some(o) = c.get("order").and_then(Value::as_i64) {
        m.insert("order".into(), json!(o));
    }
    for (src, dst) in [("createdAt", "created"), ("lastModified", "last_modified")] {
        if let Some(t) = c.get(src).and_then(Value::as_i64).and_then(|s| render_unix(s, display)) {
            m.insert(dst.into(), json!(t));
        }
    }
    for (src, dst) in [("attachmentCount", "attachment_count"), ("commentsCount", "comments_count")] {
        if let Some(n) = c.get(src).and_then(Value::as_i64).filter(|n| *n > 0) {
            m.insert(dst.into(), json!(n));
        }
    }
    Value::Object(m)
}

/// Cards of a stack, sorted by their order on the board, without deleted ones.
pub fn stack_cards(stack: &Value) -> Vec<&Value> {
    let mut cards: Vec<&Value> = stack.get("cards").and_then(Value::as_array).map(|a| a.iter().filter(|c| !is_deleted(c)).collect()).unwrap_or_default();
    cards.sort_by_key(|c| c.get("order").and_then(Value::as_i64).unwrap_or(0));
    cards
}

pub fn stack_json(s: &Value, display: Option<Tz>) -> Value {
    let cards: Vec<Value> = stack_cards(s).into_iter().map(|c| card_json(c, display)).collect();
    json!({
        "stack_id": s.get("id"),
        "title": s.get("title"),
        "order": s.get("order"),
        "card_count": cards.len(),
        "cards": cards,
    })
}

/// Find the item whose `id` equals a numeric reference, or whose title matches case-insensitively.
pub fn find_by_ref<'a>(items: &[&'a Value], reference: &Value, noun: &str) -> Result<&'a Value> {
    let id = match reference {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => bail!("{noun} must be an id or a title"),
    };
    if let Some(id) = id
        && let Some(hit) = items.iter().find(|i| i.get("id").and_then(Value::as_i64) == Some(id))
    {
        return Ok(hit);
    }
    if let Value::String(s) = reference {
        let want = s.trim().to_lowercase();
        let hits: Vec<&&Value> = items
            .iter()
            .filter(|i| i.get("title").and_then(Value::as_str).is_some_and(|t| t.trim().to_lowercase() == want))
            .collect();
        match hits.len() {
            1 => return Ok(hits[0]),
            n if n > 1 => bail!("{n} {noun}s are titled '{s}'; use the id instead"),
            _ => {}
        }
    }
    let available: Vec<String> = items
        .iter()
        .map(|i| format!("{} ({})", i.get("title").and_then(Value::as_str).unwrap_or("?"), i.get("id").map(|v| v.to_string()).unwrap_or_default()))
        .collect();
    bail!("{noun} '{}' not found; available: {}", value_text(reference), if available.is_empty() { "none".into() } else { available.join(", ") })
}

fn value_text(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

/// Convert a due-date argument into the ISO-8601 form Deck stores.
/// A date without a time means the end of that day (23:59) in `timezone` (or UTC).
pub fn due_input(input: &str, timezone: Option<&str>) -> Result<String> {
    let tz = timezone.map(|t| ical::parse_tz(t).ok_or_else(|| anyhow!("unknown timezone '{t}'"))).transpose()?;
    let local = |n: chrono::NaiveDateTime| -> Result<DateTime<Utc>> {
        Ok(match tz {
            Some(tz) => n
                .and_local_timezone(tz)
                .earliest()
                .ok_or_else(|| anyhow!("invalid local time {n} in {}", tz.name()))?
                .with_timezone(&Utc),
            None => n.and_utc(),
        })
    };
    let utc = match ical::parse_dt_input(input)? {
        DtInput::Date(d) => local(d.and_time(NaiveTime::from_hms_opt(23, 59, 0).unwrap()))?,
        DtInput::Utc(dt) => dt,
        DtInput::Local(n) => local(n)?,
    };
    Ok(utc.format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
}

/// Ids of labels/users to add and remove so that `current` becomes `wanted`.
pub fn set_diff<T: PartialEq + Clone>(current: &[T], wanted: &[T]) -> (Vec<T>, Vec<T>) {
    let add = wanted.iter().filter(|w| !current.contains(w)).cloned().collect();
    let remove = current.iter().filter(|c| !wanted.contains(c)).cloned().collect();
    (add, remove)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> Value {
        json!({
            "id": 81, "title": "Write docs", "description": "Deck section", "stackId": 12, "type": "plain",
            "lastModified": 1763030400, "createdAt": 1763030400,
            "labels": [{"id": 5, "title": "Docs", "color": "31CC7C", "boardId": 3}],
            "assignedUsers": [{"id": 1, "participant": {"primaryKey": "alice", "uid": "alice", "displayname": "Alice", "type": 0}, "cardId": 81, "type": 0}],
            "attachments": null, "attachmentCount": 0,
            "owner": {"primaryKey": "admin", "uid": "admin", "displayname": "Admin", "type": 0},
            "order": 2, "archived": false, "duedate": "2026-11-20T16:00:00+00:00", "deletedAt": 0, "commentsCount": 3
        })
    }

    #[test]
    fn request_building() {
        assert_eq!(req::boards(), ApiRequest { method: Method::GET, path: "boards?details=true".into(), body: None });
        assert_eq!(req::stacks(3, false).path, "boards/3/stacks");
        assert_eq!(req::stacks(3, true).path, "boards/3/stacks/archived");
        assert_eq!(req::card(3, 12, 81).path, "boards/3/stacks/12/cards/81");

        let c = req::create_card(3, 12, "T", "", Some("2026-11-20T16:00:00+00:00"));
        assert_eq!((c.method, c.path.as_str()), (Method::POST, "boards/3/stacks/12/cards"));
        assert_eq!(c.body.unwrap(), json!({"title": "T", "type": "plain", "order": 999, "description": "", "duedate": "2026-11-20T16:00:00+00:00"}));

        let r = req::reorder(3, 81, 14, 0);
        assert_eq!((r.method, r.path.as_str()), (Method::PUT, "boards/3/stacks/14/cards/81/reorder"));
        assert_eq!(r.body.unwrap(), json!({"order": 0, "stackId": 14}));

        assert_eq!(req::label(3, 12, 81, 5, true).path, "boards/3/stacks/12/cards/81/assignLabel");
        assert_eq!(req::label(3, 12, 81, 5, false).body.unwrap(), json!({"labelId": 5}));
        assert_eq!(req::user(3, 12, 81, "bob", false).path, "boards/3/stacks/12/cards/81/unassignUser");
        assert_eq!(req::user(3, 12, 81, "bob", true).body.unwrap(), json!({"userId": "bob"}));
        assert_eq!(req::delete_card(3, 12, 81).method, Method::DELETE);
    }

    #[test]
    fn update_keeps_unchanged_fields() {
        let mut f = CardFields::from_card(&card()).unwrap();
        assert_eq!(f.owner, "admin");
        assert_eq!(f.duedate.as_deref(), Some("2026-11-20T16:00:00+00:00"));
        f.title = "New".into();
        let r = req::update_card(3, 12, 81, &f);
        assert_eq!(r.method, Method::PUT);
        assert_eq!(
            r.body.unwrap(),
            json!({"title": "New", "type": "plain", "owner": "admin", "description": "Deck section", "order": 2, "duedate": "2026-11-20T16:00:00+00:00", "archived": false})
        );
        // Older servers return the owner as a plain user id.
        let mut c = card();
        c["owner"] = json!("bob");
        assert_eq!(CardFields::from_card(&c).unwrap().owner, "bob");
    }

    #[test]
    fn card_parsing() {
        let j = card_json(&card(), Some(chrono_tz::Europe::Berlin));
        assert_eq!(j["card_id"], 81);
        assert_eq!(j["stack_id"], 12);
        assert_eq!(j["due"], "2026-11-20T17:00:00+01:00");
        assert_eq!(j["labels"], json!([{"id": 5, "title": "Docs", "color": "31CC7C"}]));
        assert_eq!(j["assigned_users"], json!([{"id": "alice", "name": "Alice"}]));
        assert_eq!(j["owner"], "admin");
        assert_eq!(j["comments_count"], 3);
        assert!(j.get("attachment_count").is_none());
        assert_eq!(card_json(&card(), None)["due"], "2026-11-20T16:00:00Z");
    }

    #[test]
    fn stack_and_board_parsing() {
        let mut deleted = card();
        deleted["id"] = json!(82);
        deleted["deletedAt"] = json!(1763030400);
        let mut first = card();
        first["id"] = json!(80);
        first["order"] = json!(0);
        let stack = json!({"id": 12, "title": "To do", "order": 0, "cards": [card(), deleted, first]});
        let j = stack_json(&stack, None);
        assert_eq!(j["card_count"], 2);
        assert_eq!(j["cards"][0]["card_id"], 80);

        let boards = json!([
            {"id": 1, "title": "Personal", "archived": false, "deletedAt": 0, "owner": {"uid": "admin"}, "labels": [], "users": []},
            {"id": 2, "title": "Old", "archived": true, "deletedAt": 0},
            {"id": 3, "title": "Gone", "archived": false, "deletedAt": 1763030400}
        ]);
        assert_eq!(visible_boards(&boards, false).len(), 1);
        assert_eq!(visible_boards(&boards, true).len(), 2);
        let b = board_json(visible_boards(&boards, false)[0]);
        assert_eq!(b, json!({"board_id": 1, "title": "Personal", "archived": false, "owner": "admin", "labels": [], "users": []}));
    }

    #[test]
    fn reference_resolution() {
        let labels = [json!({"id": 5, "title": "Docs"}), json!({"id": 6, "title": "Bug"}), json!({"id": 7, "title": "bug"})];
        let refs: Vec<&Value> = labels.iter().collect();
        assert_eq!(find_by_ref(&refs, &json!(5), "label").unwrap()["id"], 5);
        assert_eq!(find_by_ref(&refs, &json!("6"), "label").unwrap()["id"], 6);
        assert_eq!(find_by_ref(&refs, &json!(" docs "), "label").unwrap()["id"], 5);
        let e = find_by_ref(&refs, &json!("BUG"), "label").unwrap_err().to_string();
        assert!(e.contains("2 labels"), "{e}");
        let e = find_by_ref(&refs, &json!("Feature"), "label").unwrap_err().to_string();
        assert!(e.contains("Docs (5)"), "{e}");
        assert!(find_by_ref(&refs, &json!(true), "label").is_err());
    }

    #[test]
    fn due_dates() {
        assert_eq!(due_input("2026-11-20T17:00", Some("Europe/Berlin")).unwrap(), "2026-11-20T16:00:00+00:00");
        assert_eq!(due_input("2026-11-20", Some("Europe/Berlin")).unwrap(), "2026-11-20T22:59:00+00:00");
        assert_eq!(due_input("2026-11-20T17:00:00Z", Some("Europe/Berlin")).unwrap(), "2026-11-20T17:00:00+00:00");
        assert_eq!(due_input("2026-11-20T17:00", None).unwrap(), "2026-11-20T17:00:00+00:00");
        assert!(due_input("next friday", None).is_err());
    }

    #[test]
    fn diffs() {
        assert_eq!(set_diff(&[1, 2], &[2, 3]), (vec![3], vec![1]));
        assert_eq!(set_diff::<i64>(&[], &[]), (vec![], vec![]));
    }

    #[test]
    fn error_mapping() {
        let r = req::card(3, 12, 81);
        let e = api_error(StatusCode::UNAUTHORIZED, "", &r).to_string();
        assert!(e.contains("credentials") && e.contains("app password"), "{e}");
        let e = api_error(StatusCode::FORBIDDEN, r#"{"status":403,"message":"Permission denied"}"#, &r).to_string();
        assert!(e.contains("403") && e.contains("Permission denied"), "{e}");
        let e = api_error(StatusCode::NOT_FOUND, r#"{"status":404,"message":"Card not found"}"#, &r).to_string();
        assert!(e.contains("Card not found"), "{e}");
        let e = api_error(StatusCode::NOT_FOUND, "<html>Page not found</html>", &r).to_string();
        assert!(e.contains("Deck app installed"), "{e}");
        let e = api_error(StatusCode::INTERNAL_SERVER_ERROR, r#"{"ocs":{"meta":{"message":"boom"}}}"#, &r).to_string();
        assert!(e.contains("500") && e.contains("boom"), "{e}");
    }

    #[test]
    fn api_url() {
        let d = DeckClient::new(Url::parse("https://example.com/nextcloud").unwrap(), Auth::None).unwrap();
        assert_eq!(d.url("boards/3").unwrap().as_str(), "https://example.com/nextcloud/index.php/apps/deck/api/v1.0/boards/3");
    }
}
