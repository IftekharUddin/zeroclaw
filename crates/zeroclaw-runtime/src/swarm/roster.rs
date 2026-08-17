//! The ephemeral roster: the box agents a swarm run is made of.
//!
//! A box is not a configured agent. Nothing here writes `[agents.*]`, and no
//! config reload happens — a roster is built in memory from the swarm's
//! [`BoxSpec`]s, lives for the run, and is deleted by the reap cascade.
//!
//! Two identities are minted per box and both are derived, never authored:
//!
//! * a synthetic alias, `swarm/<swarm_id>/<box_id>`. Configured aliases cannot
//!   contain `/` (see `zeroclaw_config::helpers::validate_alias_key`), so the
//!   namespace is disjoint from `[agents.*]` by construction; the builder
//!   re-checks anyway and refuses a collision.
//! * a real memory UUID from `Memory::ensure_agent_uuid`, so every row a box
//!   writes is attributed to that box and is deletable by attribution.
//!
//! The permissions envelope is the parent agent's, resolved through the same
//! [`SubAgentSpawn`] machinery as any other runtime-spawned sub-agent. A box
//! may narrow it; a box that asks for anything wider fails the whole build.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{Config, MemoryConfig};
use zeroclaw_memory::{Memory, SwarmBoxMemory, create_memory_for_swarm_box};

use crate::subagent::{SubAgentContext, SubAgentOverrides, SubAgentSpawn};
use crate::swarm::store::model::SwarmSpec;

/// First segment of every synthetic box alias.
pub const SWARM_ALIAS_PREFIX: &str = "swarm";

/// Largest roster a swarm may be built with. Boxes are concurrent agent
/// sessions under one budget; the ceiling bounds fan-out.
pub const MAX_ROSTER_SIZE: usize = 16;

/// Longest accepted `swarm_id` / `box_id`.
pub const MAX_ID_LEN: usize = 64;

/// The synthetic alias a box speaks under.
#[must_use]
pub fn swarm_box_alias(swarm_id: &str, box_id: &str) -> String {
    format!("{SWARM_ALIAS_PREFIX}/{swarm_id}/{box_id}")
}

/// Why a roster could not be built. Every variant is a refusal — there is no
/// partial roster, because a half-built one would run boxes whose siblings
/// were never validated.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RosterError {
    #[error("swarm {field} {value:?} is unusable: {reason}")]
    InvalidId {
        field: &'static str,
        value: String,
        reason: String,
    },
    #[error("swarm {swarm_id:?} has no boxes")]
    EmptyRoster { swarm_id: String },
    #[error("swarm {swarm_id:?} has {size} boxes; the maximum roster is {MAX_ROSTER_SIZE}")]
    RosterTooLarge { swarm_id: String, size: usize },
    #[error("swarm {swarm_id:?} lists box {box_id:?} more than once")]
    DuplicateBox { swarm_id: String, box_id: String },
    #[error("the synthetic alias {alias:?} collides with a configured agent")]
    AliasCollision { alias: String },
    #[error("cannot resolve the parent envelope from agent {alias:?}: {reason}")]
    Parent { alias: String, reason: String },
    #[error(
        "box {box_id:?} requested a policy outside the {parent_alias:?} envelope: {reason}; \
         a box may only narrow it"
    )]
    PolicyEscalation {
        box_id: String,
        parent_alias: String,
        reason: String,
    },
    #[error("could not mint a memory identity for {alias:?}: {reason}")]
    Identity { alias: String, reason: String },
}

/// One built box: its authored shape, its two minted identities, and the
/// validated envelope it runs inside.
#[derive(Debug, Clone)]
pub struct RosterBox {
    pub box_id: String,
    /// `swarm/<swarm_id>/<box_id>`.
    pub alias: String,
    pub role: String,
    pub job: String,
    pub slot: u8,
    /// The box's memory UUID, from `Memory::ensure_agent_uuid`.
    pub agent_id: String,
    /// Policy + config-memory allowlist, inherited from the parent agent and
    /// possibly narrowed. Cross-box memory reads do *not* ride this allowlist
    /// (it names configured aliases); they ride the UUID allowlist that
    /// [`SwarmRoster::compose_memories`] installs.
    pub context: SubAgentContext,
}

/// A validated roster. Construct it with [`build_swarm_roster`]; every box in
/// it has passed alias, envelope, and identity checks.
#[derive(Debug, Clone)]
pub struct SwarmRoster {
    swarm_id: String,
    parent_alias: String,
    boxes: Vec<RosterBox>,
}

impl SwarmRoster {
    #[must_use]
    pub fn swarm_id(&self) -> &str {
        &self.swarm_id
    }

    /// The agent whose envelope every box runs inside.
    #[must_use]
    pub fn parent_alias(&self) -> &str {
        &self.parent_alias
    }

    #[must_use]
    pub fn boxes(&self) -> &[RosterBox] {
        &self.boxes
    }

    #[must_use]
    pub fn get(&self, box_id: &str) -> Option<&RosterBox> {
        self.boxes.iter().find(|entry| entry.box_id == box_id)
    }

    #[must_use]
    pub fn box_ids(&self) -> Vec<String> {
        self.boxes
            .iter()
            .map(|entry| entry.box_id.clone())
            .collect()
    }

    /// Every box's synthetic alias, for the reap cascade.
    #[must_use]
    pub fn aliases(&self) -> Vec<String> {
        self.boxes.iter().map(|entry| entry.alias.clone()).collect()
    }

    /// Every box's memory UUID — the trust set boxes recall across.
    #[must_use]
    pub fn agent_ids(&self) -> Vec<String> {
        self.boxes
            .iter()
            .map(|entry| entry.agent_id.clone())
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// Compose one memory handle per box over the shared backend.
    ///
    /// The read allowlist is the whole roster, enumerated here rather than
    /// assembled by the caller, so "boxes read each other" cannot drift into
    /// "a box reads whatever it was handed". Writes stay self-attributed:
    /// the scoping wrapper stamps the bound box's UUID, so the allowlist
    /// widens reads only.
    #[must_use]
    pub fn compose_memories(
        &self,
        inner: &Arc<dyn Memory>,
        memory_config: &MemoryConfig,
    ) -> BTreeMap<String, SwarmBoxMemory> {
        let roster_ids = self.agent_ids();
        self.boxes
            .iter()
            .map(|entry| {
                (
                    entry.box_id.clone(),
                    create_memory_for_swarm_box(
                        Arc::clone(inner),
                        &self.swarm_id,
                        &entry.agent_id,
                        &roster_ids,
                        memory_config,
                    ),
                )
            })
            .collect()
    }
}

/// Build the roster for `spec` inside `parent_alias`'s envelope.
///
/// `policy_requests` maps `box_id` to the narrowed policy that box should run
/// under; a box with no entry inherits the parent's verbatim. Requests are
/// validated by [`SecurityPolicy::ensure_no_escalation_beyond`] through
/// [`SubAgentSpawn`], so a request for anything the parent does not already
/// hold fails the build rather than the box.
pub async fn build_swarm_roster(
    config: &Config,
    memory: &Arc<dyn Memory>,
    spec: &SwarmSpec,
    parent_alias: &str,
    policy_requests: &BTreeMap<String, SecurityPolicy>,
) -> Result<SwarmRoster, RosterError> {
    validate_id("id", &spec.id)?;
    if spec.boxes.is_empty() {
        return Err(RosterError::EmptyRoster {
            swarm_id: spec.id.clone(),
        });
    }
    if spec.boxes.len() > MAX_ROSTER_SIZE {
        return Err(RosterError::RosterTooLarge {
            swarm_id: spec.id.clone(),
            size: spec.boxes.len(),
        });
    }

    // Resolve the envelope once: every box narrows the same parent policy, so
    // re-resolving per box would let a mid-build config change hand two boxes
    // different parents.
    let parent_policy = Arc::new(SecurityPolicy::for_agent(config, parent_alias).map_err(
        |err| RosterError::Parent {
            alias: parent_alias.to_string(),
            reason: err.to_string(),
        },
    )?);

    let mut seen: HashSet<&str> = HashSet::with_capacity(spec.boxes.len());
    let mut boxes = Vec::with_capacity(spec.boxes.len());
    for box_spec in &spec.boxes {
        validate_id("box id", &box_spec.box_id)?;
        if !seen.insert(box_spec.box_id.as_str()) {
            return Err(RosterError::DuplicateBox {
                swarm_id: spec.id.clone(),
                box_id: box_spec.box_id.clone(),
            });
        }

        let alias = swarm_box_alias(&spec.id, &box_spec.box_id);
        // Structurally unreachable (configured aliases cannot contain `/`),
        // and checked anyway: a synthetic alias must never resolve to, or be
        // mistaken for, a configured agent.
        if config.agents.contains_key(&alias) {
            return Err(RosterError::AliasCollision { alias });
        }

        let spawn =
            SubAgentSpawn::for_agent_with_policy(config, parent_alias, Arc::clone(&parent_policy))
                .map_err(|err| RosterError::Parent {
                    alias: parent_alias.to_string(),
                    reason: err.to_string(),
                })?;
        let context = spawn
            .build(SubAgentOverrides {
                policy: policy_requests.get(&box_spec.box_id).cloned(),
                allowed_agent_aliases: None,
            })
            .map_err(|err| RosterError::PolicyEscalation {
                box_id: box_spec.box_id.clone(),
                parent_alias: parent_alias.to_string(),
                reason: err.to_string(),
            })?;

        let agent_id =
            memory
                .ensure_agent_uuid(&alias)
                .await
                .map_err(|err| RosterError::Identity {
                    alias: alias.clone(),
                    reason: err.to_string(),
                })?;

        boxes.push(RosterBox {
            box_id: box_spec.box_id.clone(),
            alias,
            role: box_spec.role.clone(),
            job: box_spec.job.clone(),
            slot: box_spec.slot,
            agent_id,
            context,
        });
    }

    Ok(SwarmRoster {
        swarm_id: spec.id.clone(),
        parent_alias: parent_alias.to_string(),
        boxes,
    })
}

/// Ids are alias material: they are concatenated into `swarm/<id>/<box>`, so a
/// `/` in either half would let one swarm's box forge another's alias, and a
/// `.` would open path-shaped tricks downstream. Accept only what cannot do
/// either.
fn validate_id(field: &'static str, value: &str) -> Result<(), RosterError> {
    let invalid = |reason: String| RosterError::InvalidId {
        field,
        value: value.to_string(),
        reason,
    };
    if value.is_empty() {
        return Err(invalid("must not be empty".to_string()));
    }
    if value.len() > MAX_ID_LEN {
        return Err(invalid(format!(
            "{} bytes exceeds the maximum of {MAX_ID_LEN}",
            value.len()
        )));
    }
    if let Some(ch) = value
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_'))
    {
        return Err(invalid(format!(
            "contains {ch:?}; only ASCII letters, digits, `-`, and `_` are allowed"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::store::model::BoxSpec;
    use std::path::PathBuf;
    use zeroclaw_config::schema::{AliasedAgentConfig, RiskProfileConfig};
    use zeroclaw_memory::SqliteMemory;

    fn test_config() -> Config {
        let mut config = Config::default();
        config
            .risk_profiles
            .insert("default".to_string(), RiskProfileConfig::default());
        config.agents.insert(
            "default".to_string(),
            AliasedAgentConfig {
                risk_profile: "default".into(),
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    fn spec_with_boxes(swarm_id: &str, box_ids: &[&str]) -> SwarmSpec {
        let mut spec = SwarmSpec::new(
            swarm_id,
            "test swarm",
            "anthropic",
            "model",
            "default",
            "supervisor",
            "ship it",
        );
        spec.boxes = box_ids
            .iter()
            .enumerate()
            .map(|(slot, box_id)| BoxSpec {
                box_id: (*box_id).to_string(),
                role: "worker".to_string(),
                job: "do the thing".to_string(),
                slot: u8::try_from(slot).unwrap_or(0),
            })
            .collect();
        spec
    }

    fn temp_memory() -> (tempfile::TempDir, Arc<dyn Memory>) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mem = SqliteMemory::new("sqlite", tmp.path()).expect("sqlite memory");
        (tmp, Arc::new(mem))
    }

    #[tokio::test]
    async fn builds_synthetic_aliases_and_real_uuids() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1", "box-2"]);

        let roster = build_swarm_roster(&config, &memory, &spec, "default", &BTreeMap::new())
            .await
            .expect("a roster inside the parent envelope must build");

        assert_eq!(roster.len(), 2);
        assert_eq!(
            roster.aliases(),
            vec!["swarm/swarm-1/box-1", "swarm/swarm-1/box-2"]
        );
        let ids = roster.agent_ids();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "each box gets its own identity");
        for id in &ids {
            assert!(
                uuid::Uuid::parse_str(id).is_ok(),
                "expected a real memory UUID, got {id:?}"
            );
        }
        // The identities are stable: rebuilding resolves the same rows.
        let again = build_swarm_roster(&config, &memory, &spec, "default", &BTreeMap::new())
            .await
            .expect("rebuild must succeed");
        assert_eq!(again.agent_ids(), ids);
    }

    #[tokio::test]
    async fn boxes_inherit_the_parent_envelope_verbatim() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1"]);
        let parent = SecurityPolicy::for_agent(&config, "default").expect("parent policy");

        let roster = build_swarm_roster(&config, &memory, &spec, "default", &BTreeMap::new())
            .await
            .expect("build");

        let ctx = &roster.boxes()[0].context;
        assert_eq!(ctx.parent_alias, "default");
        assert_eq!(ctx.policy.autonomy, parent.autonomy);
        assert_eq!(ctx.policy.allowed_roots, parent.allowed_roots);
        assert_eq!(ctx.policy.allowed_commands, parent.allowed_commands);
    }

    #[tokio::test]
    async fn a_narrowing_policy_request_is_accepted() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1"]);

        let mut narrowed = SecurityPolicy::for_agent(&config, "default").expect("parent policy");
        narrowed.allowed_commands.clear();
        narrowed.max_actions_per_hour = 1;
        let requests = BTreeMap::from([("box-1".to_string(), narrowed)]);

        let roster = build_swarm_roster(&config, &memory, &spec, "default", &requests)
            .await
            .expect("narrowing must be allowed");
        assert_eq!(roster.boxes()[0].context.policy.max_actions_per_hour, 1);
        assert!(roster.boxes()[0].context.policy.allowed_commands.is_empty());
    }

    #[tokio::test]
    async fn a_widening_policy_request_fails_the_build_closed() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1", "box-2"]);

        let mut wider = SecurityPolicy::for_agent(&config, "default").expect("parent policy");
        wider.allowed_roots.push(PathBuf::from("/secrets"));
        let requests = BTreeMap::from([("box-2".to_string(), wider)]);

        let err = build_swarm_roster(&config, &memory, &spec, "default", &requests)
            .await
            .expect_err("escalation must fail closed");
        match err {
            RosterError::PolicyEscalation { box_id, .. } => assert_eq!(box_id, "box-2"),
            other => panic!("expected a policy-escalation refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_escalation_axis_fails_the_build() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1"]);
        let parent = SecurityPolicy::for_agent(&config, "default").expect("parent policy");

        let mut wider_commands = parent.clone();
        wider_commands.allowed_commands.push("rm".to_string());
        let mut wider_actions = parent.clone();
        wider_actions.max_actions_per_hour = parent.max_actions_per_hour + 1;
        let mut wider_cost = parent.clone();
        wider_cost.max_cost_per_day_cents = parent.max_cost_per_day_cents + 1;
        let mut wider_env = parent.clone();
        wider_env
            .shell_env_passthrough
            .push("AWS_SECRET".to_string());

        for (label, policy) in [
            ("commands", wider_commands),
            ("actions", wider_actions),
            ("cost", wider_cost),
            ("env passthrough", wider_env),
        ] {
            let requests = BTreeMap::from([("box-1".to_string(), policy)]);
            let err = build_swarm_roster(&config, &memory, &spec, "default", &requests)
                .await
                .expect_err("escalation must fail closed");
            assert!(
                matches!(err, RosterError::PolicyEscalation { .. }),
                "widening {label} must be refused, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn synthetic_aliases_cannot_shadow_a_configured_agent() {
        let mut config = test_config();
        // Force the collision the alias grammar makes unreachable, to prove the
        // builder refuses rather than binding a box to a configured agent.
        config.agents.insert(
            "swarm/swarm-1/box-1".to_string(),
            AliasedAgentConfig {
                risk_profile: "default".into(),
                ..AliasedAgentConfig::default()
            },
        );
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1"]);

        let err = build_swarm_roster(&config, &memory, &spec, "default", &BTreeMap::new())
            .await
            .expect_err("a shadowing alias must be refused");
        assert_eq!(
            err,
            RosterError::AliasCollision {
                alias: "swarm/swarm-1/box-1".to_string()
            }
        );
    }

    #[tokio::test]
    async fn ids_that_could_forge_another_boxs_alias_are_refused() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();

        // `swarm/a/b` + `c` and `swarm/a` + `b/c` would both render
        // `swarm/a/b/c` if `/` were allowed in either half.
        for (swarm_id, box_id) in [
            ("a/b", "c"),
            ("a", "b/c"),
            ("..", "box-1"),
            ("a", ".."),
            ("a b", "box-1"),
            ("", "box-1"),
            ("a", ""),
        ] {
            let spec = spec_with_boxes(swarm_id, &[box_id]);
            let err = build_swarm_roster(&config, &memory, &spec, "default", &BTreeMap::new())
                .await
                .expect_err("unusable ids must be refused");
            assert!(
                matches!(err, RosterError::InvalidId { .. }),
                "({swarm_id:?}, {box_id:?}) must be refused, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn duplicate_and_oversized_rosters_are_refused() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();

        let dupes = spec_with_boxes("swarm-1", &["box-1", "box-1"]);
        assert!(matches!(
            build_swarm_roster(&config, &memory, &dupes, "default", &BTreeMap::new()).await,
            Err(RosterError::DuplicateBox { .. })
        ));

        let mut empty = spec_with_boxes("swarm-1", &[]);
        empty.boxes.clear();
        assert!(matches!(
            build_swarm_roster(&config, &memory, &empty, "default", &BTreeMap::new()).await,
            Err(RosterError::EmptyRoster { .. })
        ));

        let ids: Vec<String> = (0..=MAX_ROSTER_SIZE).map(|i| format!("box-{i}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let too_many = spec_with_boxes("swarm-1", &refs);
        assert!(matches!(
            build_swarm_roster(&config, &memory, &too_many, "default", &BTreeMap::new()).await,
            Err(RosterError::RosterTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn an_unknown_parent_agent_is_refused() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1"]);

        let err = build_swarm_roster(&config, &memory, &spec, "ghost", &BTreeMap::new())
            .await
            .expect_err("an unknown parent must be refused");
        assert!(matches!(err, RosterError::Parent { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn building_a_roster_writes_no_agents_config() {
        let config = test_config();
        let (_tmp, memory) = temp_memory();
        let spec = spec_with_boxes("swarm-1", &["box-1", "box-2"]);
        let before: Vec<String> = config.agents.keys().cloned().collect();

        let roster = build_swarm_roster(&config, &memory, &spec, "default", &BTreeMap::new())
            .await
            .expect("build");

        let after: Vec<String> = config.agents.keys().cloned().collect();
        assert_eq!(before, after, "a roster must never add configured agents");
        for entry in roster.boxes() {
            assert!(!config.agents.contains_key(&entry.alias));
        }
    }
}
