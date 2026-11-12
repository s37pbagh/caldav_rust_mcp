//! MCP tools for Nextcloud Deck boards, stacks and cards.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::deck::{self, CardFields, DeckClient, req};
use crate::tools::{TZ_ARG, Tools};

const BOARD_ARG: &str = "Board id or title (from deck_list_boards).";
const STACK_HINT_ARG: &str = "Optional: id or title of the stack (column) that holds the card. Saves a lookup; without it the card is found by scanning the board.";
const LABELS_ARG: &str = "Labels the card should have, as label ids or titles of the board's labels (see deck_list_boards). Replaces the current labels; [] removes all.";
const USERS_ARG: &str = "Nextcloud user ids that should be assigned to the card. Replaces the current assignees; [] removes all. The users must have access to the board.";
const DUE_ARG: &str = "Due date: YYYY-MM-DD (means 23:59 that day), YYYY-MM-DDTHH:MM (local time in `timezone`) or RFC 3339 with offset.";

pub fn definitions() -> Vec<Value> {
    let card_locator = |extra: Value| -> Value {
        let mut props = json!({
            "board": {"type": ["integer", "string"], "description": BOARD_ARG},
            "card_id": {"type": "integer", "description": "Card id (from deck_list_stacks)."},
            "stack": {"type": ["integer", "string"], "description": STACK_HINT_ARG}
        });
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        props
    };
    vec![
        json!({
            "name": "deck_list_boards",
            "description": "List the Nextcloud Deck boards the user can see, with their labels and users (assignable people).",
            "inputSchema": {"type": "object", "properties": {
                "include_archived": {"type": "boolean", "description": "Also list archived boards (default false)."}
            }},
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "deck_list_stacks",
            "description": "List the stacks (columns) of a Deck board in board order, each with its cards (title, description, due date, labels, assignees).",
            "inputSchema": {"type": "object", "properties": {
                "board": {"type": ["integer", "string"], "description": BOARD_ARG},
                "archived": {"type": "boolean", "description": "List archived cards instead of active ones (default false)."},
                "timezone": {"type": "string", "description": TZ_ARG}
            }, "required": ["board"]},
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "deck_get_card",
            "description": "Get one Deck card with all its details.",
            "inputSchema": {"type": "object", "properties": card_locator(json!({
                "timezone": {"type": "string", "description": TZ_ARG}
            })), "required": ["board", "card_id"]},
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "deck_create_card",
            "description": "Create a card at the bottom of a Deck stack. Requires board, stack and title.",
            "inputSchema": {"type": "object", "properties": {
                "board": {"type": ["integer", "string"], "description": BOARD_ARG},
                "stack": {"type": ["integer", "string"], "description": "Id or title of the stack (column) to put the card in."},
                "title": {"type": "string"},
                "description": {"type": "string", "description": "Card description (Markdown)."},
                "due": {"type": "string", "description": DUE_ARG},
                "labels": {"type": "array", "items": {"type": ["integer", "string"]}, "description": LABELS_ARG},
                "assigned_users": {"type": "array", "items": {"type": "string"}, "description": USERS_ARG},
                "timezone": {"type": "string", "description": TZ_ARG}
            }, "required": ["board", "stack", "title"]}
        }),
        json!({
            "name": "deck_update_card",
            "description": "Change fields of a Deck card. Only the given fields change; everything else is kept. Use deck_move_card to move it to another stack.",
            "inputSchema": {"type": "object", "properties": card_locator(json!({
                "title": {"type": "string"},
                "description": {"type": "string", "description": "New description (Markdown); \"\" clears it."},
                "due": {"type": "string", "description": format!("{DUE_ARG} \"\" removes the due date.")},
                "archived": {"type": "boolean", "description": "Archive (true) or unarchive (false) the card."},
                "labels": {"type": "array", "items": {"type": ["integer", "string"]}, "description": LABELS_ARG},
                "assigned_users": {"type": "array", "items": {"type": "string"}, "description": USERS_ARG},
                "timezone": {"type": "string", "description": TZ_ARG}
            })), "required": ["board", "card_id"]}
        }),
        json!({
            "name": "deck_move_card",
            "description": "Move a Deck card to another stack (column) or to another position in its stack.",
            "inputSchema": {"type": "object", "properties": card_locator(json!({
                "to_stack": {"type": ["integer", "string"], "description": "Id or title of the target stack. May be the card's current stack to reorder it."},
                "position": {"type": "integer", "minimum": 0, "description": "0-based position in the target stack (0 = top). Defaults to the bottom."},
                "timezone": {"type": "string", "description": TZ_ARG}
            })), "required": ["board", "card_id", "to_stack"]}
        }),
        json!({
            "name": "deck_delete_card",
            "description": "Delete a Deck card.",
            "inputSchema": {"type": "object", "properties": card_locator(json!({})), "required": ["board", "card_id"]},
            "annotations": {"destructiveHint": true}
        }),
    ]
}

fn id_arg(args: &Value, key: &str) -> Result<i64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| anyhow!("{key} is required and must be a numeric id"))
}

fn ref_arg<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| match v {
        Value::String(s) => !s.trim().is_empty(),
        Value::Number(_) => true,
        _ => false,
    })
}

/// A card located on a board.
struct Located {
    board: i64,
    stack: i64,
    card: Value,
}

impl Tools {
    fn deck(&self) -> Result<&DeckClient> {
        self.deck.as_ref().ok_or_else(|| anyhow!("Deck tools are disabled: set NEXTCLOUD_URL to the Nextcloud server root to enable them"))
    }

    pub(crate) async fn call_deck(&self, name: &str, args: &Value) -> Result<Value> {
        let d = self.deck()?;
        match name {
            "deck_list_boards" => {
                let include_archived = args.get("include_archived").and_then(Value::as_bool).unwrap_or(false);
                let boards = d.send(req::boards()).await?;
                let list: Vec<Value> = deck::visible_boards(&boards, include_archived).into_iter().map(deck::board_json).collect();
                Ok(json!({ "count": list.len(), "boards": list }))
            }
            "deck_list_stacks" => self.deck_list_stacks(d, args).await,
            "deck_get_card" => {
                let loc = self.locate_card(d, args).await?;
                Ok(deck::card_json(&loc.card, self.display_tz(args)?))
            }
            "deck_create_card" => self.deck_create_card(d, args).await,
            "deck_update_card" => self.deck_update_card(d, args).await,
            "deck_move_card" => self.deck_move_card(d, args).await,
            "deck_delete_card" => {
                let loc = self.locate_card(d, args).await?;
                let card_id = loc.card.get("id").and_then(Value::as_i64).unwrap_or_default();
                d.send(req::delete_card(loc.board, loc.stack, card_id)).await?;
                Ok(json!({ "deleted": true, "card_id": card_id, "title": loc.card.get("title") }))
            }
            other => bail!("unknown tool '{other}'"),
        }
    }

    /// Resolve the `board` argument (id or title) to a board id.
    async fn board_id(&self, d: &DeckClient, args: &Value) -> Result<i64> {
        let reference = ref_arg(args, "board").ok_or_else(|| anyhow!("board is required (id or title)"))?;
        if let Some(id) = reference.as_i64().or_else(|| reference.as_str().and_then(|s| s.trim().parse().ok())) {
            return Ok(id);
        }
        let boards = d.send(req::boards()).await?;
        let visible = deck::visible_boards(&boards, true);
        let board = deck::find_by_ref(&visible, reference, "board")?;
        board.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("board has no id"))
    }

    async fn stacks(&self, d: &DeckClient, board: i64, archived: bool) -> Result<Vec<Value>> {
        let v = d.send(req::stacks(board, archived)).await?;
        let mut stacks = v.as_array().cloned().unwrap_or_default();
        stacks.sort_by_key(|s| s.get("order").and_then(Value::as_i64).unwrap_or(0));
        Ok(stacks)
    }

    async fn board_labels(&self, d: &DeckClient, board: i64) -> Result<Vec<Value>> {
        let b = d.send(req::board(board)).await?;
        Ok(b.get("labels").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// Resolve a `labels` argument to label ids before anything is written.
    async fn label_ids(&self, d: &DeckClient, board: i64, args: &Value) -> Result<Option<Vec<i64>>> {
        let Some(wanted) = args.get("labels") else { return Ok(None) };
        let wanted = wanted.as_array().ok_or_else(|| anyhow!("labels must be an array of label ids or titles"))?;
        if wanted.is_empty() {
            return Ok(Some(vec![]));
        }
        let labels = self.board_labels(d, board).await?;
        let refs: Vec<&Value> = labels.iter().collect();
        let mut ids = Vec::new();
        for w in wanted {
            let id = deck::find_by_ref(&refs, w, "label")?.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("label has no id"))?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(Some(ids))
    }

    fn user_ids(args: &Value) -> Result<Option<Vec<String>>> {
        let Some(v) = args.get("assigned_users") else { return Ok(None) };
        let arr = v.as_array().ok_or_else(|| anyhow!("assigned_users must be an array of user ids"))?;
        let mut out: Vec<String> = Vec::new();
        for u in arr {
            let id = u.as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("assigned_users must contain user id strings"))?;
            if !out.iter().any(|o| o == id) {
                out.push(id.to_string());
            }
        }
        Ok(Some(out))
    }

    /// Find the card given by `board` + `card_id` (+ optional `stack` hint), searching active then archived cards.
    async fn locate_card(&self, d: &DeckClient, args: &Value) -> Result<Located> {
        let board = self.board_id(d, args).await?;
        let card_id = id_arg(args, "card_id")?;
        if let Some(stack) = ref_arg(args, "stack")
            && let Some(stack) = stack.as_i64().or_else(|| stack.as_str().and_then(|s| s.trim().parse().ok()))
        {
            let card = d.send(req::card(board, stack, card_id)).await?;
            let stack = card.get("stackId").and_then(Value::as_i64).unwrap_or(stack);
            return Ok(Located { board, stack, card });
        }
        for archived in [false, true] {
            for s in self.stacks(d, board, archived).await? {
                if let Some(card) = deck::stack_cards(&s).into_iter().find(|c| c.get("id").and_then(Value::as_i64) == Some(card_id)) {
                    let stack = card.get("stackId").and_then(Value::as_i64).or_else(|| s.get("id").and_then(Value::as_i64)).unwrap_or_default();
                    return Ok(Located { board, stack, card: card.clone() });
                }
            }
        }
        bail!("card {card_id} not found on board {board}")
    }

    /// Re-read a card after changing it.
    async fn fetch_card(&self, d: &DeckClient, board: i64, stack: i64, card: i64, args: &Value) -> Result<Value> {
        let c = d.send(req::card(board, stack, card)).await?;
        Ok(deck::card_json(&c, self.display_tz(args)?))
    }

    async fn apply_labels_users(
        &self,
        d: &DeckClient,
        loc: (i64, i64, i64),
        card: &Value,
        labels: Option<Vec<i64>>,
        users: Option<Vec<String>>,
    ) -> Result<()> {
        let (board, stack, id) = loc;
        if let Some(wanted) = labels {
            let current: Vec<i64> = card.get("labels").and_then(Value::as_array).map(|a| a.iter().filter_map(|l| l.get("id").and_then(Value::as_i64)).collect()).unwrap_or_default();
            let (add, remove) = deck::set_diff(&current, &wanted);
            for l in remove {
                d.send(req::label(board, stack, id, l, false)).await?;
            }
            for l in add {
                d.send(req::label(board, stack, id, l, true)).await?;
            }
        }
        if let Some(wanted) = users {
            let current: Vec<String> = card
                .get("assignedUsers")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|u| deck::user_id(u.get("participant").unwrap_or(u))).collect())
                .unwrap_or_default();
            let (add, remove) = deck::set_diff(&current, &wanted);
            for u in remove {
                d.send(req::user(board, stack, id, &u, false)).await?;
            }
            for u in add {
                d.send(req::user(board, stack, id, &u, true)).await.map_err(|e| e.context(format!("could not assign user '{u}'")))?;
            }
        }
        Ok(())
    }

    async fn deck_list_stacks(&self, d: &DeckClient, args: &Value) -> Result<Value> {
        let board = self.board_id(d, args).await?;
        let archived = args.get("archived").and_then(Value::as_bool).unwrap_or(false);
        let display = self.display_tz(args)?;
        let stacks: Vec<Value> = self.stacks(d, board, archived).await?.iter().map(|s| deck::stack_json(s, display)).collect();
        Ok(json!({ "board_id": board, "archived": archived, "stacks": stacks }))
    }

    async fn deck_create_card(&self, d: &DeckClient, args: &Value) -> Result<Value> {
        let title = args.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("title is required"))?;
        let board = self.board_id(d, args).await?;
        let stack_ref = ref_arg(args, "stack").ok_or_else(|| anyhow!("stack is required (id or title)"))?;
        let stacks = self.stacks(d, board, false).await?;
        let stack = deck::find_by_ref(&stacks.iter().collect::<Vec<_>>(), stack_ref, "stack")?.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("stack has no id"))?;
        let due = args.get("due").and_then(Value::as_str).filter(|v| !v.trim().is_empty()).map(|v| deck::due_input(v, self.tz(args))).transpose()?;
        // Validate labels and users before creating anything.
        let labels = self.label_ids(d, board, args).await?;
        let users = Self::user_ids(args)?;

        let description = args.get("description").and_then(Value::as_str).unwrap_or("");
        let card = d.send(req::create_card(board, stack, title, description, due.as_deref())).await?;
        let id = card.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("Deck did not return the new card's id"))?;
        self.apply_labels_users(d, (board, stack, id), &card, labels, users)
            .await
            .map_err(|e| e.context(format!("card {id} was created, but setting its labels/assignees failed")))?;
        self.fetch_card(d, board, stack, id, args).await
    }

    async fn deck_update_card(&self, d: &DeckClient, args: &Value) -> Result<Value> {
        let loc = self.locate_card(d, args).await?;
        let id = loc.card.get("id").and_then(Value::as_i64).unwrap_or_default();
        let mut fields = CardFields::from_card(&loc.card)?;
        let before = fields.clone();
        if let Some(t) = args.get("title").and_then(Value::as_str) {
            let t = t.trim();
            if t.is_empty() {
                bail!("title cannot be empty");
            }
            fields.title = t.to_string();
        }
        if let Some(desc) = args.get("description").and_then(Value::as_str) {
            fields.description = desc.to_string();
        }
        if let Some(due) = args.get("due").and_then(Value::as_str) {
            fields.duedate = if due.trim().is_empty() { None } else { Some(deck::due_input(due, self.tz(args))?) };
        }
        if let Some(a) = args.get("archived").and_then(Value::as_bool) {
            fields.archived = a;
        }
        let labels = self.label_ids(d, loc.board, args).await?;
        let users = Self::user_ids(args)?;

        if fields != before {
            let updated = d.send(req::update_card(loc.board, loc.stack, id, &fields)).await?;
            if args.get("archived").and_then(Value::as_bool).is_some()
                && let Some(a) = updated.get("archived").and_then(Value::as_bool)
                && a != fields.archived
            {
                bail!("the Deck server did not change the archived state of card {id}; this Deck version may not support archiving through the API");
            }
        }
        self.apply_labels_users(d, (loc.board, loc.stack, id), &loc.card, labels, users).await?;
        self.fetch_card(d, loc.board, loc.stack, id, args).await
    }

    async fn deck_move_card(&self, d: &DeckClient, args: &Value) -> Result<Value> {
        let loc = self.locate_card(d, args).await?;
        let id = loc.card.get("id").and_then(Value::as_i64).unwrap_or_default();
        let target_ref = ref_arg(args, "to_stack").ok_or_else(|| anyhow!("to_stack is required (id or title)"))?;
        let stacks = self.stacks(d, loc.board, false).await?;
        let target = deck::find_by_ref(&stacks.iter().collect::<Vec<_>>(), target_ref, "stack")?;
        let to_stack = target.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("stack has no id"))?;
        let others = deck::stack_cards(target).into_iter().filter(|c| c.get("id").and_then(Value::as_i64) != Some(id)).count() as i64;
        let position = match args.get("position") {
            None | Some(Value::Null) => others,
            Some(p) => p.as_i64().filter(|p| *p >= 0).ok_or_else(|| anyhow!("position must be a non-negative integer"))?.min(others),
        };
        d.send(req::reorder(loc.board, id, to_stack, position)).await?;
        let mut out = self.fetch_card(d, loc.board, to_stack, id, args).await?;
        if out.get("stack_id").and_then(Value::as_i64) != Some(to_stack) {
            bail!("the Deck server did not move card {id} to stack {to_stack}; it is still in stack {}", out.get("stack_id").unwrap_or(&Value::Null));
        }
        out["moved_from_stack_id"] = json!(loc.stack);
        Ok(out)
    }
}
