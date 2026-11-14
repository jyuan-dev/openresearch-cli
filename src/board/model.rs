//! Board records and the closed vocabularies stored in their TEXT columns.

use serde::{Deserialize, Serialize};

use crate::error::{anyhow, Result};

/// Implements `as_str` / `parse` for a fieldless enum whose stored form is a fixed word.
macro_rules! stored_words {
    ($ty:ident { $($variant:ident => $word:literal),+ $(,)? }) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $word),+ }
            }

            pub fn parse(word: &str) -> Result<Self> {
                match word {
                    $($word => Ok(Self::$variant),)+
                    other => Err(anyhow!(
                        "`{other}` is not a {}; expected one of: {}",
                        stringify!($ty),
                        [$($word),+].join(", ")
                    )),
                }
            }
        }

        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketStatus {
    Proposed,
    Ready,
    Running,
    Waiting,
    Review,
    Done,
    Blocked,
    Rejected,
    Cancelled,
}

stored_words!(TicketStatus {
    Proposed => "proposed",
    Ready => "ready",
    Running => "running",
    Waiting => "waiting",
    Review => "review",
    Done => "done",
    Blocked => "blocked",
    Rejected => "rejected",
    Cancelled => "cancelled",
});

impl TicketStatus {
    /// Board columns in display order.
    pub const COLUMNS: [TicketStatus; 9] = [
        Self::Proposed,
        Self::Ready,
        Self::Running,
        Self::Waiting,
        Self::Review,
        Self::Blocked,
        Self::Done,
        Self::Rejected,
        Self::Cancelled,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketKind {
    /// Backed by exactly one experiment node.
    Experiment,
    /// Reading, analysis or writing work with no experiment node.
    Analysis,
}

stored_words!(TicketKind {
    Experiment => "experiment",
    Analysis => "analysis",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EpicStatus {
    Proposed,
    Active,
    Answered,
    Dropped,
}

stored_words!(EpicStatus {
    Proposed => "proposed",
    Active => "active",
    Answered => "answered",
    Dropped => "dropped",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Supports,
    Refutes,
    Inconclusive,
}

stored_words!(Outcome {
    Supports => "supports",
    Refutes => "refutes",
    Inconclusive => "inconclusive",
});

/// Who wrote a record. `UserChat` is an experiment the user asked for in a chat turn of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProposedBy {
    User,
    Agent,
    UserChat,
}

stored_words!(ProposedBy {
    User => "user",
    Agent => "agent",
    UserChat => "user-chat",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    ApproveBatch,
    AcceptBatch,
    AcceptOne,
    Question,
    Blocked,
    Budget,
    PublishGate,
    Import,
    CloseEpic,
    ApproveCriteria,
}

stored_words!(DecisionKind {
    ApproveBatch => "approve_batch",
    AcceptBatch => "accept_batch",
    AcceptOne => "accept_one",
    Question => "question",
    Blocked => "blocked",
    Budget => "budget",
    PublishGate => "publish_gate",
    Import => "import",
    CloseEpic => "close_epic",
    ApproveCriteria => "approve_criteria",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    Open,
    Resolved,
    /// The tickets it asked about moved on without it.
    Superseded,
}

stored_words!(DecisionStatus {
    Open => "open",
    Resolved => "resolved",
    Superseded => "superseded",
});

/// Who is acting. Only the user decides; agents propose and report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    User,
    /// An agent chat, by session id.
    Agent(String),
    /// The board itself, optionally citing the epic rule that allowed it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the scheduler acts as the board")
    )]
    System(Option<String>),
}

impl Actor {
    /// The CLI's caller: inside an agent chat, orx sets `ORX_CHAT_SESSION_ID`.
    pub fn from_environment() -> Self {
        match crate::local::chat::launching_chat_session() {
            Some(session) => Self::Agent(session),
            None => Self::User,
        }
    }

    pub fn stored(&self) -> String {
        match self {
            Self::User => "user".into(),
            Self::Agent(session) => format!("agent:{session}"),
            Self::System(None) => "system".into(),
            Self::System(Some(rule)) => format!("system:{rule}"),
        }
    }
}

/// Per-epic standing permissions. Every rule is off by default: the user approves each ticket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EpicRules {
    pub auto_refill_same_batch: bool,
    pub auto_seed_rerun: bool,
    pub undo_window_min: u32,
    pub repair_cap: u32,
    pub auto_done: bool,
    pub negative_in_batch: bool,
    pub overrun_factor: f64,
    pub overrun_pauses_dispatch: bool,
}

impl Default for EpicRules {
    fn default() -> Self {
        Self {
            auto_refill_same_batch: false,
            auto_seed_rerun: false,
            undo_window_min: 15,
            repair_cap: 3,
            auto_done: false,
            negative_in_batch: true,
            overrun_factor: 1.5,
            overrun_pauses_dispatch: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardProject {
    pub project_id: String,
    pub enabled: bool,
    pub priority: i64,
    pub confidential: bool,
    pub deadline_ms: Option<i64>,
    pub key_prefix: String,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Epic {
    pub id: String,
    pub project_id: String,
    pub key: String,
    pub title: String,
    pub question: String,
    pub status: EpicStatus,
    pub priority: i64,
    /// The approved answer-criteria version; 0 until the user approves one.
    pub criteria_version: i64,
    pub rules: EpicRules,
    pub budget_gpu_hours: Option<f64>,
    pub budget_deadline_ms: Option<i64>,
    pub conclusion: Option<String>,
    pub proposed_by: ProposedBy,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Criteria {
    pub epic_id: String,
    pub version: i64,
    pub body_md: String,
    pub checks: serde_json::Value,
    pub approved_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub id: String,
    pub project_id: String,
    pub epic_id: Option<String>,
    pub key: String,
    pub kind: TicketKind,
    pub batch_id: Option<String>,
    pub title: String,
    pub hypothesis: Option<String>,
    pub change_spec: Option<String>,
    pub status: TicketStatus,
    pub priority: i64,
    pub base_experiment_id: Option<String>,
    pub experiment_id: Option<String>,
    pub needs_gpus: i64,
    pub est_minutes: Option<i64>,
    pub validation: Option<serde_json::Value>,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    pub machine_id: Option<String>,
    pub repair_count: i64,
    pub outcome: Option<Outcome>,
    pub core_hypothesis: bool,
    pub result: Option<serde_json::Value>,
    pub conclusion: Option<String>,
    pub tainted: bool,
    pub tainted_reason: Option<String>,
    pub proposed_by: ProposedBy,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub id: String,
    pub project_id: String,
    pub epic_id: Option<String>,
    pub kind: DecisionKind,
    pub ticket_ids: Vec<String>,
    pub payload: serde_json::Value,
    pub status: DecisionStatus,
    pub resolution: Option<serde_json::Value>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub id: String,
    pub name: String,
    /// `local` or `ssh:<alias>`, the compute backend runs on it use.
    pub backend: String,
    pub gpu_slots: i64,
    /// Owned by the user: the only machines a confidential project may run on.
    pub own: bool,
    pub enabled: bool,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardEvent {
    pub id: i64,
    pub at: i64,
    pub project_id: String,
    pub subject_id: String,
    pub actor: String,
    pub kind: String,
    pub detail: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_words_round_trip_and_reject_unknown_words() {
        for status in TicketStatus::COLUMNS {
            assert_eq!(TicketStatus::parse(status.as_str()).unwrap(), status);
        }
        assert_eq!(
            ProposedBy::parse("user-chat").unwrap(),
            ProposedBy::UserChat
        );
        let error = TicketStatus::parse("doing").unwrap_err().to_string();
        assert!(error.contains("doing") && error.contains("proposed"));
    }

    #[test]
    fn epic_rules_default_to_per_ticket_approval_and_fill_missing_fields() {
        let rules: EpicRules = serde_json::from_str(r#"{"autoDone":true}"#).unwrap();
        assert!(rules.auto_done);
        assert!(!rules.auto_refill_same_batch && !rules.auto_seed_rerun);
        assert_eq!(rules.repair_cap, 3);
        assert!(rules.negative_in_batch);
    }
}
