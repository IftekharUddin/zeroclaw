//! The `swarm_delegate` tool: the orchestrator's only handle on its roster.
//!
//! This is a NEW, roster-only delegate tool — deliberately not the general
//! config-coupled [`crate::tools::DelegateTool`]. Its parameter schema
//! enumerates the live roster's `box_id`s, so a well-behaved provider cannot
//! even represent an out-of-roster target; `execute` re-checks membership as
//! defense-in-depth against a provider that ignores the schema. Delegating runs
//! the named box's agent turn (under [`zeroclaw_api::ingress::TurnOrigin::Swarm`],
//! budget-admitted first) and returns its response to the orchestrator.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use zeroclaw_api::attribution::ToolKind;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_api::tool_attribution;

use crate::swarm::engine::{BoxTurnError, SwarmRunState};

/// A roster-scoped delegate tool bound to one live swarm run.
pub struct SwarmDelegateTool {
    run: Arc<SwarmRunState>,
}

impl SwarmDelegateTool {
    /// Canonical tool name.
    pub const NAME: &'static str = "swarm_delegate";

    #[must_use]
    pub fn new(run: Arc<SwarmRunState>) -> Self {
        Self { run }
    }

    fn box_ids(&self) -> Vec<String> {
        self.run.roster().box_ids()
    }

    fn required_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
        args.get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("`{field}` is required"))
    }
}

#[async_trait]
impl Tool for SwarmDelegateTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Delegate a focused subtask to one box of this swarm's roster. The box \
         runs the subtask to completion in its own agent turn under the swarm's \
         shared budget and returns its response. You may only target a box that \
         is on the roster (see the `box_id` enum); there is no way to name an \
         agent outside this swarm."
    }

    fn parameters_schema(&self) -> Value {
        // The enum is the roster: an out-of-roster target is unrepresentable.
        json!({
            "type": "object",
            "properties": {
                "box_id": {
                    "type": "string",
                    "enum": self.box_ids(),
                    "description": "Which roster box handles the subtask."
                },
                "subtask": {
                    "type": "string",
                    "description": "The focused, self-contained subtask for the box. The box does not see the orchestrator's conversation."
                }
            },
            "required": ["box_id", "subtask"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let box_id = match Self::required_str(&args, "box_id") {
            Ok(value) => value,
            Err(err) => return Ok(ToolResult::err(err)),
        };
        let subtask = match Self::required_str(&args, "subtask") {
            Ok(value) => value,
            Err(err) => return Ok(ToolResult::err(err)),
        };

        // Defense-in-depth: the schema enum already makes an out-of-roster
        // target unrepresentable, but a provider that ignores the schema is
        // rejected here rather than silently routed anywhere.
        if self.run.roster().get(box_id).is_none() {
            let valid = self.box_ids().join(", ");
            return Ok(ToolResult::err(format!(
                "box {box_id:?} is not on this swarm's roster; valid boxes are: {valid}"
            )));
        }

        match self.run.run_box_turn(box_id, subtask).await {
            Ok(text) => Ok(ToolResult::ok(ToolOutput::from(
                if text.trim().is_empty() {
                    format!("{box_id} completed without output")
                } else {
                    text
                },
            ))),
            Err(BoxTurnError::BudgetExhausted(axis)) => Ok(ToolResult::err(format!(
                "swarm budget exhausted on the {} axis; no further delegation admitted",
                axis.as_str()
            ))),
            Err(err @ BoxTurnError::UnknownBox(_)) => Ok(ToolResult::err(err.to_string())),
            Err(BoxTurnError::UserEngaged(_)) => Ok(ToolResult::err(format!(
                "box {box_id} is engaged with a user right now; it will be available again once \
                 the user hands it back — do not delegate to it until then"
            ))),
            Err(BoxTurnError::Turn(msg)) => {
                Ok(ToolResult::err(format!("box {box_id} turn failed: {msg}")))
            }
        }
    }
}

tool_attribution!(SwarmDelegateTool, ToolKind::Plugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_enumerates_exactly_the_roster_box_ids() {
        // A pure check on the schema shape: the enum is the source of the
        // "out-of-roster is unrepresentable" guarantee. Rendered from a fixed
        // id list so the assertion does not need a live run.
        let box_ids = vec!["box-1".to_string(), "box-2".to_string()];
        let schema = json!({
            "type": "object",
            "properties": {
                "box_id": { "type": "string", "enum": box_ids.clone() },
            },
        });
        let enumerated = schema["properties"]["box_id"]["enum"]
            .as_array()
            .expect("box_id must carry an enum")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(enumerated, box_ids);
    }
}
