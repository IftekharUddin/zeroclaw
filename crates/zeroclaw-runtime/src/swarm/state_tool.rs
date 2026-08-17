//! The `swarm_state` tool: a box's only handle on the shared board.
//!
//! The tool is constructed per box with the swarm id and box id already bound,
//! so the model can say *what* it is doing but never *who* is doing it. There
//! is no argument that selects another box, another swarm, or another holder's
//! claim; impersonation is not validated away, it is unrepresentable.
//!
//! It is deliberately not registered anywhere in the global tool tables — box
//! tool assembly hands it out, and only to boxes.

use async_trait::async_trait;
use serde_json::{Value, json};
use zeroclaw_api::attribution::ToolKind;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_api::tool_attribution;

use crate::swarm::board::{BoxStatus, ClaimOutcome, ReleaseOutcome, SwarmStateBoard};

/// Board access for one box of one swarm.
pub struct SwarmStateTool {
    board: SwarmStateBoard,
    swarm_id: String,
    box_id: String,
}

impl SwarmStateTool {
    pub const NAME: &'static str = "swarm_state";

    #[must_use]
    pub fn new(
        board: SwarmStateBoard,
        swarm_id: impl Into<String>,
        box_id: impl Into<String>,
    ) -> Self {
        Self {
            board,
            swarm_id: swarm_id.into(),
            box_id: box_id.into(),
        }
    }

    /// The box this tool speaks as. Fixed at construction.
    #[must_use]
    pub fn box_id(&self) -> &str {
        &self.box_id
    }

    fn required_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
        args.get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("`{field}` is required"))
    }

    fn publish(&self, args: &Value) -> ToolResult {
        let status_label = match Self::required_str(args, "status") {
            Ok(value) => value,
            Err(err) => return ToolResult::err(err),
        };
        let Some(status) = BoxStatus::parse(status_label) else {
            return ToolResult::err(format!(
                "`status` must be one of {}, got {status_label:?}",
                BoxStatus::all().join(", ")
            ));
        };
        let note = args.get("note").and_then(Value::as_str).unwrap_or("");

        match self
            .board
            .publish(&self.swarm_id, &self.box_id, status, note)
        {
            Ok(state) => ToolResult::ok(ToolOutput::json_with_text(
                json!({
                    "box_id": self.box_id,
                    "status": state.status,
                    "claim": state.claim,
                    "note": state.note,
                }),
                format!("{} is now {}", self.box_id, state.status.as_str()),
            )),
            Err(err) => ToolResult::err(err.to_string()),
        }
    }

    fn read_board(&self) -> ToolResult {
        let Some(snapshot) = self.board.snapshot(&self.swarm_id) else {
            return ToolResult::err(format!("no board for swarm {:?}", self.swarm_id));
        };
        let lines: Vec<String> = snapshot
            .boxes
            .iter()
            .map(|(box_id, state)| {
                let claim = state
                    .claim
                    .as_deref()
                    .map_or_else(String::new, |key| format!(" holding `{key}`"));
                let note = if state.note.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", state.note)
                };
                format!("{box_id}: {}{claim}{note}", state.status.as_str())
            })
            .collect();
        let data = match serde_json::to_value(&snapshot) {
            Ok(data) => data,
            Err(err) => return ToolResult::err(format!("could not render the board: {err}")),
        };
        ToolResult::ok(ToolOutput::json_with_text(data, lines.join("\n")))
    }

    fn claim(&self, args: &Value) -> ToolResult {
        let task_key = match Self::required_str(args, "task_key") {
            Ok(value) => value,
            Err(err) => return ToolResult::err(err),
        };
        match self.board.claim(&self.swarm_id, &self.box_id, task_key) {
            Ok(ClaimOutcome::Granted) => ToolResult::ok(ToolOutput::json_with_text(
                json!({ "task_key": task_key, "granted": true, "holder": self.box_id }),
                format!("claimed `{task_key}`"),
            )),
            Ok(ClaimOutcome::Held { holder }) => ToolResult::ok(ToolOutput::json_with_text(
                json!({ "task_key": task_key, "granted": false, "holder": holder }),
                format!("`{task_key}` is already held by {holder}; pick different work"),
            )),
            Err(err) => ToolResult::err(err.to_string()),
        }
    }

    fn release(&self, args: &Value) -> ToolResult {
        let task_key = match Self::required_str(args, "task_key") {
            Ok(value) => value,
            Err(err) => return ToolResult::err(err),
        };
        match self.board.release(&self.swarm_id, &self.box_id, task_key) {
            Ok(ReleaseOutcome::Released) => ToolResult::ok(ToolOutput::json_with_text(
                json!({ "task_key": task_key, "released": true }),
                format!("released `{task_key}`"),
            )),
            Ok(ReleaseOutcome::NotHolder { holder }) => ToolResult::ok(ToolOutput::json_with_text(
                json!({ "task_key": task_key, "released": false, "holder": holder }),
                format!("`{task_key}` is held by {holder}; only its holder can release it"),
            )),
            Ok(ReleaseOutcome::Unclaimed) => ToolResult::ok(ToolOutput::json_with_text(
                json!({ "task_key": task_key, "released": false }),
                format!("`{task_key}` was not claimed"),
            )),
            Err(err) => ToolResult::err(err.to_string()),
        }
    }
}

#[async_trait]
impl Tool for SwarmStateTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Coordinate with the other boxes of this swarm through the shared state board. \
         Actions: `publish` your status (idle|working|blocked|done) and a short note; \
         `read_board` to see every box's status and the live claims; `claim` a task_key \
         before starting work on it, which succeeds only if no other box holds it; \
         `release` a task_key you hold when you are done with it. You always act as your \
         own box — there is no way to publish, claim, or release on another box's behalf."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["publish", "read_board", "claim", "release"],
                    "description": "What to do on the board."
                },
                "status": {
                    "type": "string",
                    "enum": BoxStatus::all(),
                    "description": "Required for `publish`: what you are doing now."
                },
                "note": {
                    "type": "string",
                    "description": "Optional for `publish`: one line of detail for the other boxes."
                },
                "task_key": {
                    "type": "string",
                    "description": "Required for `claim` and `release`: the work item's key."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let action = match Self::required_str(&args, "action") {
            Ok(value) => value,
            Err(err) => return Ok(ToolResult::err(err)),
        };
        Ok(match action {
            "publish" => self.publish(&args),
            "read_board" => self.read_board(),
            "claim" => self.claim(&args),
            "release" => self.release(&args),
            other => ToolResult::err(format!(
                "unknown action {other:?}; expected publish, read_board, claim, or release"
            )),
        })
    }
}

tool_attribution!(SwarmStateTool, ToolKind::Plugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_for(box_id: &str, board: &SwarmStateBoard) -> SwarmStateTool {
        SwarmStateTool::new(board.clone(), "s1", box_id)
    }

    fn board() -> SwarmStateBoard {
        let board = SwarmStateBoard::new();
        board
            .register("s1", ["box-1".to_string(), "box-2".to_string()])
            .expect("register");
        board
    }

    #[tokio::test]
    async fn publish_then_read_board_round_trips() {
        let board = board();
        let one = tool_for("box-1", &board);
        let two = tool_for("box-2", &board);

        let published = one
            .execute(json!({ "action": "publish", "status": "working", "note": "on task-a" }))
            .await
            .expect("execute");
        assert!(published.success, "{:?}", published.error);

        let read = two
            .execute(json!({ "action": "read_board" }))
            .await
            .expect("execute");
        assert!(read.success);
        let data = read.output.data().expect("read_board returns structure");
        assert_eq!(data["boxes"]["box-1"]["status"], "working");
        assert_eq!(data["boxes"]["box-1"]["note"], "on task-a");
        assert!(read.output.as_str().contains("box-1: working"));
    }

    #[tokio::test]
    async fn a_box_can_only_act_as_itself() {
        let board = board();
        let one = tool_for("box-1", &board);

        // Extra arguments naming another box are inert: the tool has no
        // parameter that could redirect the action.
        one.execute(json!({
            "action": "publish",
            "status": "done",
            "box_id": "box-2",
            "swarm_id": "other",
        }))
        .await
        .expect("execute");

        let snapshot = board.snapshot("s1").expect("board");
        assert_eq!(snapshot.boxes["box-1"].status, BoxStatus::Done);
        assert_eq!(
            snapshot.boxes["box-2"].status,
            BoxStatus::Idle,
            "no argument may let one box publish as another"
        );
    }

    #[tokio::test]
    async fn contested_claims_report_the_holder_and_only_the_holder_releases() {
        let board = board();
        let one = tool_for("box-1", &board);
        let two = tool_for("box-2", &board);

        let won = one
            .execute(json!({ "action": "claim", "task_key": "task-a" }))
            .await
            .expect("execute");
        assert_eq!(won.output.data().expect("data")["granted"], json!(true));

        let lost = two
            .execute(json!({ "action": "claim", "task_key": "task-a" }))
            .await
            .expect("execute");
        let lost_data = lost.output.data().expect("data");
        assert_eq!(lost_data["granted"], json!(false));
        assert_eq!(lost_data["holder"], json!("box-1"));

        let steal = two
            .execute(json!({ "action": "release", "task_key": "task-a" }))
            .await
            .expect("execute");
        assert_eq!(steal.output.data().expect("data")["released"], json!(false));
        assert_eq!(
            board.snapshot("s1").expect("board").claims["task-a"],
            "box-1",
            "a non-holder must not free another box's claim"
        );

        let released = one
            .execute(json!({ "action": "release", "task_key": "task-a" }))
            .await
            .expect("execute");
        assert_eq!(
            released.output.data().expect("data")["released"],
            json!(true)
        );
        assert!(board.snapshot("s1").expect("board").claims.is_empty());
    }

    #[tokio::test]
    async fn malformed_calls_are_refused_without_touching_the_board() {
        let board = board();
        let one = tool_for("box-1", &board);

        for args in [
            json!({}),
            json!({ "action": "escalate" }),
            json!({ "action": "publish" }),
            json!({ "action": "publish", "status": "omniscient" }),
            json!({ "action": "claim" }),
            json!({ "action": "release", "task_key": "   " }),
        ] {
            let result = one.execute(args.clone()).await.expect("execute");
            assert!(!result.success, "{args} must be refused");
            assert!(result.error.is_some());
        }

        let snapshot = board.snapshot("s1").expect("board");
        assert!(snapshot.claims.is_empty());
        assert_eq!(snapshot.boxes["box-1"], Default::default());
    }
}
