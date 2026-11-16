//! Board records: epics, tickets, the user's decisions and the activity log.
//!
//! Every write that changes a ticket or decision appends to `board_events`
//! in the same transaction, so the log is the dated record of who decided what.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{json, Value};

use super::{now_ms, Store};
use crate::board::checks::{self, Checks};
use crate::board::gate;
use crate::board::import;
use crate::board::scheduler::{self, Candidate, MachineSlots};
use crate::board::transitions::{self, TicketAction};
use crate::board::{
    Actor, BoardEvent, BoardProject, Criteria, Decision, DecisionKind, DecisionStatus, Epic,
    EpicRules, EpicStatus, Machine, Outcome, ProposedBy, Ticket, TicketKind, TicketStatus,
};
use crate::error::{anyhow, bail, Context, Result};

pub(super) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("board.sql"))
        .context("creating the board tables")?;
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct BoardSettingsUpdate {
    pub enabled: Option<bool>,
    pub key_prefix: Option<String>,
    pub priority: Option<i64>,
    pub confidential: Option<bool>,
    pub deadline_ms: Option<Option<i64>>,
}

#[derive(Clone, Debug)]
pub struct NewEpic {
    pub project_id: String,
    pub title: String,
    pub question: String,
    pub priority: i64,
    pub budget_gpu_hours: Option<f64>,
    pub budget_deadline_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct NewTicket {
    pub project_id: String,
    pub epic_id: Option<String>,
    pub kind: TicketKind,
    pub batch_id: Option<String>,
    pub title: String,
    pub hypothesis: Option<String>,
    pub change_spec: Option<String>,
    pub priority: i64,
    pub base_experiment_id: Option<String>,
    pub needs_gpus: i64,
    pub est_minutes: Option<i64>,
    pub validation: Option<Value>,
    pub harness: Option<String>,
    pub core_hypothesis: bool,
}

/// What a resolved decision chose, as the user gave it.
#[derive(Clone, Debug, Default)]
pub struct DecisionChoice {
    /// Tickets the user kept selected on a batch card.
    pub selected: Option<Vec<String>>,
    /// The option picked on a question card.
    pub option: Option<String>,
    pub text: Option<String>,
    /// The grouping the user settled on an import card.
    pub groups: Option<Vec<import::Group>>,
}

const TICKET_COLUMNS: &str = "id, project_id, epic_id, key, kind, batch_id, title, hypothesis,
    change_spec, status, priority, base_experiment_id, experiment_id, needs_gpus, est_minutes,
    validation_json, harness, session_id, machine_id, repair_count, outcome, core_hypothesis,
    result_json, conclusion, tainted, tainted_reason, proposed_by, started_at, finished_at,
    created_at, updated_at";

const EPIC_COLUMNS: &str = "id, project_id, key, title, question, status, priority,
    criteria_version, rules_json, budget_gpu_hours, budget_deadline_ms, conclusion, proposed_by,
    created_at, updated_at";

const DECISION_COLUMNS: &str = "id, project_id, epic_id, kind, ticket_ids_json, payload_json,
    status, resolution_json, created_at, resolved_at";

fn json_column(row: &Row, index: usize) -> rusqlite::Result<Option<Value>> {
    let text: Option<String> = row.get(index)?;
    text.map(|text| {
        serde_json::from_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    })
    .transpose()
}

fn word<T>(row: &Row, index: usize, parse: fn(&str) -> Result<T>) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    parse(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            error.into_boxed_dyn_error(),
        )
    })
}

fn ticket_from_row(row: &Row) -> rusqlite::Result<Ticket> {
    Ok(Ticket {
        id: row.get(0)?,
        project_id: row.get(1)?,
        epic_id: row.get(2)?,
        key: row.get(3)?,
        kind: word(row, 4, TicketKind::parse)?,
        batch_id: row.get(5)?,
        title: row.get(6)?,
        hypothesis: row.get(7)?,
        change_spec: row.get(8)?,
        status: word(row, 9, TicketStatus::parse)?,
        priority: row.get(10)?,
        base_experiment_id: row.get(11)?,
        experiment_id: row.get(12)?,
        needs_gpus: row.get(13)?,
        est_minutes: row.get(14)?,
        validation: json_column(row, 15)?,
        harness: row.get(16)?,
        session_id: row.get(17)?,
        machine_id: row.get(18)?,
        repair_count: row.get(19)?,
        outcome: row
            .get::<_, Option<String>>(20)?
            .map(|text| Outcome::parse(&text))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    20,
                    rusqlite::types::Type::Text,
                    error.into_boxed_dyn_error(),
                )
            })?,
        core_hypothesis: row.get(21)?,
        result: json_column(row, 22)?,
        conclusion: row.get(23)?,
        tainted: row.get(24)?,
        tainted_reason: row.get(25)?,
        proposed_by: word(row, 26, ProposedBy::parse)?,
        started_at: row.get(27)?,
        finished_at: row.get(28)?,
        created_at: row.get(29)?,
        updated_at: row.get(30)?,
    })
}

fn epic_from_row(row: &Row) -> rusqlite::Result<Epic> {
    let rules: Value = json_column(row, 8)?.unwrap_or(Value::Null);
    Ok(Epic {
        id: row.get(0)?,
        project_id: row.get(1)?,
        key: row.get(2)?,
        title: row.get(3)?,
        question: row.get(4)?,
        status: word(row, 5, EpicStatus::parse)?,
        priority: row.get(6)?,
        criteria_version: row.get(7)?,
        rules: serde_json::from_value(rules).unwrap_or_default(),
        budget_gpu_hours: row.get(9)?,
        budget_deadline_ms: row.get(10)?,
        conclusion: row.get(11)?,
        proposed_by: word(row, 12, ProposedBy::parse)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn decision_from_row(row: &Row) -> rusqlite::Result<Decision> {
    let ticket_ids = json_column(row, 4)?.unwrap_or_else(|| json!([]));
    Ok(Decision {
        id: row.get(0)?,
        project_id: row.get(1)?,
        epic_id: row.get(2)?,
        kind: word(row, 3, DecisionKind::parse)?,
        ticket_ids: serde_json::from_value(ticket_ids).unwrap_or_default(),
        payload: json_column(row, 5)?.unwrap_or(Value::Null),
        status: word(row, 6, DecisionStatus::parse)?,
        resolution: json_column(row, 7)?,
        created_at: row.get(8)?,
        resolved_at: row.get(9)?,
    })
}

fn detail_reason(detail: &Value) -> Value {
    detail.get("reason").cloned().unwrap_or(Value::Null)
}

/// Adds `extra`'s keys to `payload`, merging one level of nested objects.
fn merge_payload(payload: &mut Value, extra: Value) {
    let (Some(target), Value::Object(extra)) = (payload.as_object_mut(), extra) else {
        return;
    };
    for (key, value) in extra {
        match (target.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(more)) => existing.extend(more),
            (_, value) => {
                target.insert(key, value);
            }
        }
    }
}

fn proposer(actor: &Actor) -> ProposedBy {
    match actor {
        Actor::Agent(_) => ProposedBy::Agent,
        _ => ProposedBy::User,
    }
}

fn require_user(actor: &Actor, what: &str) -> Result<()> {
    if *actor != Actor::User {
        bail!("Only the user can {what}. Agents propose with `orx ticket propose` or ask with `orx ticket ask`.");
    }
    Ok(())
}

/// A ticket-key prefix from a project name: the initials of its words, or its first letters.
pub fn default_key_prefix(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let prefix: String = if words.len() > 1 {
        words
            .iter()
            .filter_map(|word| word.chars().next())
            .collect()
    } else {
        words
            .first()
            .map(|word| word.chars().take(3).collect())
            .unwrap_or_default()
    };
    let prefix: String = prefix
        .to_ascii_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect();
    if prefix.is_empty() {
        "T".into()
    } else {
        prefix
    }
}

fn valid_key_prefix(prefix: &str) -> Result<String> {
    let prefix = prefix.trim().to_ascii_uppercase();
    if prefix.is_empty()
        || prefix.len() > 6
        || !prefix.chars().all(|c| c.is_ascii_alphanumeric())
        || !prefix.starts_with(|c: char| c.is_ascii_alphabetic())
    {
        bail!("A ticket prefix is 1–6 letters or digits starting with a letter, like `SJ`; `{prefix}` is not.");
    }
    Ok(prefix)
}

impl Store {
    fn append_board_event(
        conn: &Connection,
        project_id: &str,
        subject_id: &str,
        actor: &Actor,
        kind: &str,
        detail: &Value,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO board_events (at, project_id, subject_id, actor, kind, detail_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now_ms(),
                project_id,
                subject_id,
                actor.stored(),
                kind,
                detail.to_string()
            ],
        )?;
        Ok(())
    }

    pub fn board_project(&self, project_id: &str) -> Result<Option<BoardProject>> {
        Ok(self
            .conn
            .query_row(
                "SELECT project_id, enabled, priority, confidential, deadline_ms, key_prefix, updated_at
                 FROM board_projects WHERE project_id = ?1",
                [project_id],
                |row| {
                    Ok(BoardProject {
                        project_id: row.get(0)?,
                        enabled: row.get(1)?,
                        priority: row.get(2)?,
                        confidential: row.get(3)?,
                        deadline_ms: row.get(4)?,
                        key_prefix: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    /// Creates or updates a project's board settings. The first call picks a key prefix.
    pub fn update_board_project(
        &self,
        project_id: &str,
        update: &BoardSettingsUpdate,
    ) -> Result<BoardProject> {
        let project = self.get_local_project(project_id)?.ok_or_else(|| {
            anyhow!("No local project `{project_id}`. List projects with `orx projects`.")
        })?;
        let prefix = update
            .key_prefix
            .as_deref()
            .map(valid_key_prefix)
            .transpose()?;
        let tx = self.conn.unchecked_transaction()?;
        let existing = self.board_project(project_id)?;
        let prefix = match (prefix, &existing) {
            (Some(prefix), _) => prefix,
            (None, Some(existing)) => existing.key_prefix.clone(),
            (None, None) => default_key_prefix(&project.name),
        };
        let taken: Option<String> = tx
            .query_row(
                "SELECT project_id FROM board_projects WHERE key_prefix = ?1 AND project_id != ?2",
                params![prefix, project_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(other) = taken {
            bail!("The ticket prefix `{prefix}` is already used by project {other}; pick another with `--prefix`.");
        }
        let now = now_ms();
        tx.execute(
            "INSERT INTO board_projects (project_id, key_prefix, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(project_id) DO UPDATE SET key_prefix = ?2, updated_at = ?3",
            params![project_id, prefix, now],
        )?;
        if let Some(enabled) = update.enabled {
            tx.execute(
                "UPDATE board_projects SET enabled = ?2 WHERE project_id = ?1",
                params![project_id, enabled],
            )?;
        }
        if let Some(priority) = update.priority {
            tx.execute(
                "UPDATE board_projects SET priority = ?2 WHERE project_id = ?1",
                params![project_id, priority],
            )?;
        }
        if let Some(confidential) = update.confidential {
            tx.execute(
                "UPDATE board_projects SET confidential = ?2 WHERE project_id = ?1",
                params![project_id, confidential],
            )?;
        }
        if let Some(deadline) = update.deadline_ms {
            tx.execute(
                "UPDATE board_projects SET deadline_ms = ?2 WHERE project_id = ?1",
                params![project_id, deadline],
            )?;
        }
        Self::append_board_event(
            &tx,
            project_id,
            project_id,
            &Actor::User,
            "board_settings",
            &json!({
                "enabled": update.enabled, "prefix": prefix, "priority": update.priority,
                "confidential": update.confidential, "deadlineMs": update.deadline_ms,
            }),
        )?;
        tx.commit()?;
        self.board_project(project_id)?
            .ok_or_else(|| anyhow!("board settings for {project_id} were not saved"))
    }

    fn require_board_project(&self, project_id: &str) -> Result<BoardProject> {
        self.board_project(project_id)?.ok_or_else(|| {
            anyhow!("Project {project_id} has no board yet. Start one with `orx board enable {project_id}`.")
        })
    }

    pub fn create_epic(&self, new: &NewEpic, actor: &Actor) -> Result<Epic> {
        let board = self.require_board_project(&new.project_id)?;
        if new.title.trim().is_empty() || new.question.trim().is_empty() {
            bail!("An epic needs a title and the research question it answers.");
        }
        let tx = self.conn.unchecked_transaction()?;
        let seq: i64 = tx.query_row(
            "UPDATE board_projects SET next_epic = next_epic + 1 WHERE project_id = ?1
             RETURNING next_epic - 1",
            [&new.project_id],
            |row| row.get(0),
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!("{}-E{seq}", board.key_prefix);
        let now = now_ms();
        tx.execute(
            "INSERT INTO board_epics (id, project_id, key, title, question, status, priority,
                 rules_json, budget_gpu_hours, budget_deadline_ms, proposed_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'proposed', ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                id,
                new.project_id,
                key,
                new.title.trim(),
                new.question.trim(),
                new.priority,
                serde_json::to_string(&EpicRules::default())?,
                new.budget_gpu_hours,
                new.budget_deadline_ms,
                proposer(actor).as_str(),
                now
            ],
        )?;
        Self::append_board_event(
            &tx,
            &new.project_id,
            &id,
            actor,
            "epic_created",
            &json!({"key": key}),
        )?;
        tx.commit()?;
        self.epic(&id)?
            .ok_or_else(|| anyhow!("epic {key} was not saved"))
    }

    /// An epic by id or key (`SJ-E3`).
    pub fn epic(&self, id_or_key: &str) -> Result<Option<Epic>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {EPIC_COLUMNS} FROM board_epics WHERE id = ?1 OR key = ?2"),
                params![id_or_key, id_or_key.to_ascii_uppercase()],
                epic_from_row,
            )
            .optional()?)
    }

    pub fn require_epic(&self, id_or_key: &str) -> Result<Epic> {
        self.epic(id_or_key)?.ok_or_else(|| {
            anyhow!("No epic `{id_or_key}`. List them with `orx board view <projectId>`.")
        })
    }

    pub fn list_epics(&self, project_id: &str) -> Result<Vec<Epic>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {EPIC_COLUMNS} FROM board_epics WHERE project_id = ?1
             ORDER BY priority, created_at, CAST(substr(key, instr(key, '-E') + 2) AS INTEGER)"
        ))?;
        let epics = statement
            .query_map([project_id], epic_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(epics)
    }

    pub fn set_epic_rules(&self, epic_id: &str, rules: &EpicRules, actor: &Actor) -> Result<Epic> {
        require_user(actor, "change an epic's rules")?;
        let epic = self.require_epic(epic_id)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE board_epics SET rules_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![epic.id, serde_json::to_string(rules)?, now_ms()],
        )?;
        Self::append_board_event(
            &tx,
            &epic.project_id,
            &epic.id,
            actor,
            "epic_rules",
            &serde_json::to_value(rules)?,
        )?;
        tx.commit()?;
        self.require_epic(&epic.id)
    }

    /// Adds the next answer-criteria version and asks the user to approve it.
    pub fn propose_criteria(
        &self,
        epic_id: &str,
        body_md: &str,
        checks: &Value,
        actor: &Actor,
    ) -> Result<Criteria> {
        let epic = self.require_epic(epic_id)?;
        if body_md.trim().is_empty() {
            bail!(
                "Answer criteria need a body: what result answers {}?",
                epic.key
            );
        }
        let tx = self.conn.unchecked_transaction()?;
        let version: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM board_epic_criteria WHERE epic_id = ?1",
            [&epic.id],
            |row| row.get(0),
        )?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO board_epic_criteria (epic_id, version, body_md, checks_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![epic.id, version, body_md.trim(), checks.to_string(), now],
        )?;
        // A newer draft replaces any older one still waiting for the user.
        tx.execute(
            "UPDATE board_decisions SET status = 'superseded', resolved_at = ?2
             WHERE epic_id = ?1 AND kind = 'approve_criteria' AND status = 'open'",
            params![epic.id, now],
        )?;
        Self::insert_decision(
            &tx,
            &epic.project_id,
            Some(&epic.id),
            DecisionKind::ApproveCriteria,
            &[],
            &json!({"version": version}),
            &format!("criteria:{}:{version}", epic.id),
        )?;
        Self::append_board_event(
            &tx,
            &epic.project_id,
            &epic.id,
            actor,
            "criteria_proposed",
            &json!({"version": version}),
        )?;
        tx.commit()?;
        self.criteria(&epic.id, version)?
            .ok_or_else(|| anyhow!("criteria v{version} of {} were not saved", epic.key))
    }

    pub fn criteria(&self, epic_id: &str, version: i64) -> Result<Option<Criteria>> {
        Ok(self
            .conn
            .query_row(
                "SELECT epic_id, version, body_md, checks_json, approved_at, created_at
                 FROM board_epic_criteria WHERE epic_id = ?1 AND version = ?2",
                params![epic_id, version],
                |row| {
                    Ok(Criteria {
                        epic_id: row.get(0)?,
                        version: row.get(1)?,
                        body_md: row.get(2)?,
                        checks: json_column(row, 3)?.unwrap_or(Value::Null),
                        approved_at: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn latest_criteria(&self, epic_id: &str) -> Result<Option<Criteria>> {
        let version: Option<i64> = self.conn.query_row(
            "SELECT MAX(version) FROM board_epic_criteria WHERE epic_id = ?1",
            [epic_id],
            |row| row.get(0),
        )?;
        match version {
            Some(version) => self.criteria(epic_id, version),
            None => Ok(None),
        }
    }

    /// Locks `version` as the epic's answer criteria and activates a proposed epic.
    pub fn approve_criteria(&self, epic_id: &str, version: i64, actor: &Actor) -> Result<Epic> {
        require_user(actor, "approve an epic's answer criteria")?;
        let epic = self.require_epic(epic_id)?;
        if self.criteria(&epic.id, version)?.is_none() {
            bail!(
                "{} has no criteria v{version}. Show them with `orx epic show {}`.",
                epic.key,
                epic.key
            );
        }
        if matches!(epic.status, EpicStatus::Answered | EpicStatus::Dropped) {
            bail!("{} is {}; its criteria are closed.", epic.key, epic.status);
        }
        let tx = self.conn.unchecked_transaction()?;
        let now = now_ms();
        tx.execute(
            "UPDATE board_epic_criteria SET approved_at = ?3 WHERE epic_id = ?1 AND version = ?2",
            params![epic.id, version, now],
        )?;
        tx.execute(
            "UPDATE board_epics SET criteria_version = ?2,
                 status = CASE status WHEN 'proposed' THEN 'active' ELSE status END, updated_at = ?3
             WHERE id = ?1",
            params![epic.id, version, now],
        )?;
        tx.execute(
            "UPDATE board_decisions SET status = 'resolved', resolved_at = ?2, resolution_json = ?3
             WHERE dedupe_key = ?1 AND status = 'open'",
            params![
                format!("criteria:{}:{version}", epic.id),
                now,
                json!({"approved": true}).to_string()
            ],
        )?;
        Self::append_board_event(
            &tx,
            &epic.project_id,
            &epic.id,
            actor,
            "criteria_approved",
            &json!({"version": version}),
        )?;
        tx.commit()?;
        self.require_epic(&epic.id)
    }

    pub fn set_epic_status(
        &self,
        epic_id: &str,
        status: EpicStatus,
        conclusion: Option<&str>,
        actor: &Actor,
    ) -> Result<Epic> {
        require_user(actor, "close or drop an epic")?;
        let epic = self.require_epic(epic_id)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE board_epics SET status = ?2, conclusion = COALESCE(?3, conclusion), updated_at = ?4 WHERE id = ?1",
            params![epic.id, status.as_str(), conclusion, now_ms()],
        )?;
        Self::append_board_event(
            &tx,
            &epic.project_id,
            &epic.id,
            actor,
            "epic_status",
            &json!({"status": status, "conclusion": conclusion}),
        )?;
        tx.commit()?;
        self.require_epic(&epic.id)
    }

    /// Files a new ticket as `proposed` and puts it on its batch's approval card.
    pub fn create_ticket(&self, new: &NewTicket, actor: &Actor) -> Result<Ticket> {
        let board = self.require_board_project(&new.project_id)?;
        if new.title.trim().is_empty() {
            bail!("A ticket needs a title.");
        }
        if let Some(epic_id) = &new.epic_id {
            let epic = self.require_epic(epic_id)?;
            if epic.project_id != new.project_id {
                bail!("Epic {} belongs to another project.", epic.key);
            }
            if matches!(epic.status, EpicStatus::Answered | EpicStatus::Dropped) {
                bail!(
                    "Epic {} is {}; propose the ticket under an open epic.",
                    epic.key,
                    epic.status
                );
            }
        }
        if new.kind == TicketKind::Analysis && new.needs_gpus > 0 {
            bail!("An analysis ticket runs no experiment, so it cannot reserve GPUs; make it an experiment ticket.");
        }
        let tx = self.conn.unchecked_transaction()?;
        let seq: i64 = tx.query_row(
            "UPDATE board_projects SET next_ticket = next_ticket + 1 WHERE project_id = ?1
             RETURNING next_ticket - 1",
            [&new.project_id],
            |row| row.get(0),
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!("{}-{seq:03}", board.key_prefix);
        let now = now_ms();
        tx.execute(
            "INSERT INTO board_tickets (id, project_id, epic_id, key, kind, batch_id, title,
                 hypothesis, change_spec, status, priority, base_experiment_id, needs_gpus,
                 est_minutes, validation_json, harness, core_hypothesis, proposed_by,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'proposed', ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?18)",
            params![
                id,
                new.project_id,
                new.epic_id
                    .as_ref()
                    .map(|epic| self.require_epic(epic).map(|epic| epic.id))
                    .transpose()?,
                key,
                new.kind.as_str(),
                new.batch_id,
                new.title.trim(),
                new.hypothesis,
                new.change_spec,
                new.priority,
                new.base_experiment_id,
                new.needs_gpus,
                new.est_minutes,
                new.validation.as_ref().map(Value::to_string),
                new.harness,
                new.core_hypothesis,
                proposer(actor).as_str(),
                now
            ],
        )?;
        Self::append_board_event(
            &tx,
            &new.project_id,
            &id,
            actor,
            "ticket_proposed",
            &json!({"key": key}),
        )?;
        // One approval card per batch, or per ticket when it was proposed alone.
        let card = new.batch_id.clone().unwrap_or_else(|| id.clone());
        let epic_id = new
            .epic_id
            .as_ref()
            .map(|epic| self.require_epic(epic).map(|epic| epic.id))
            .transpose()?;
        Self::add_to_card(
            &tx,
            &new.project_id,
            epic_id.as_deref(),
            DecisionKind::ApproveBatch,
            &format!("approve:{}:{card}", new.project_id),
            &id,
            json!({"batch": new.batch_id}),
        )?;
        tx.commit()?;
        self.require_ticket(&id)
    }

    /// Fills in or corrects what a proposed ticket declares before approval.
    /// Agents may amend: the ticket still waits for the user on its card.
    pub fn amend_ticket(
        &self,
        id_or_key: &str,
        est_minutes: Option<i64>,
        validation: Option<Value>,
        needs_gpus: Option<i64>,
        actor: &Actor,
    ) -> Result<Ticket> {
        let ticket = self.require_ticket(id_or_key)?;
        if ticket.status != TicketStatus::Proposed {
            bail!(
                "{} is {}; only a proposed ticket can be amended.",
                ticket.key,
                ticket.status
            );
        }
        if let Some(gpus) = needs_gpus {
            if gpus < 0 {
                bail!("`--gpus` cannot be negative.");
            }
            if ticket.kind == TicketKind::Analysis && gpus > 0 {
                bail!("An analysis ticket runs no experiment, so it cannot reserve GPUs.");
            }
        }
        if validation.as_ref().is_some_and(|value| !value.is_object()) {
            bail!("The validation must be a JSON object.");
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE board_tickets SET
                 est_minutes = COALESCE(?2, est_minutes),
                 validation_json = COALESCE(?3, validation_json),
                 needs_gpus = COALESCE(?4, needs_gpus),
                 updated_at = ?5
             WHERE id = ?1",
            params![
                ticket.id,
                est_minutes,
                validation.as_ref().map(Value::to_string),
                needs_gpus,
                now_ms()
            ],
        )?;
        Self::append_board_event(
            &tx,
            &ticket.project_id,
            &ticket.id,
            actor,
            "ticket_amended",
            &json!({"estMinutes": est_minutes, "validation": validation, "gpus": needs_gpus}),
        )?;
        tx.commit()?;
        self.require_ticket(&ticket.id)
    }

    /// A ticket by id or key (`SJ-042`).
    pub fn ticket(&self, id_or_key: &str) -> Result<Option<Ticket>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {TICKET_COLUMNS} FROM board_tickets WHERE id = ?1 OR key = ?2"),
                params![id_or_key, id_or_key.to_ascii_uppercase()],
                ticket_from_row,
            )
            .optional()?)
    }

    pub fn require_ticket(&self, id_or_key: &str) -> Result<Ticket> {
        self.ticket(id_or_key)?.ok_or_else(|| {
            anyhow!("No ticket `{id_or_key}`. List them with `orx board view <projectId>`.")
        })
    }

    pub fn list_tickets(&self, project_id: &str) -> Result<Vec<Ticket>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {TICKET_COLUMNS} FROM board_tickets WHERE project_id = ?1
             ORDER BY priority, created_at"
        ))?;
        let tickets = statement
            .query_map([project_id], ticket_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(tickets)
    }

    /// Why `ticket` cannot be approved yet, if anything is missing.
    fn ready_gaps(&self, ticket: &Ticket) -> Result<Vec<String>> {
        let mut gaps = Vec::new();
        if ticket.kind == TicketKind::Experiment {
            if ticket.est_minutes.is_none() {
                gaps.push(format!(
                    "an estimated duration (`orx ticket amend {} --est 2h`)",
                    ticket.key
                ));
            }
            if ticket.validation.is_none() {
                gaps.push(format!(
                    "its validation metrics and logs (`orx ticket amend {} --validation-file …`)",
                    ticket.key
                ));
            }
        }
        if let Some(epic_id) = &ticket.epic_id {
            let epic = self.require_epic(epic_id)?;
            if epic.criteria_version == 0 {
                gaps.push(format!(
                    "approved answer criteria on {} (`orx epic approve {}`)",
                    epic.key, epic.key
                ));
            }
        }
        Ok(gaps)
    }

    /// Moves a ticket by one step of the board's state machine and logs it.
    pub fn move_ticket(
        &self,
        id_or_key: &str,
        action: TicketAction,
        actor: &Actor,
        detail: Value,
    ) -> Result<Ticket> {
        let ticket = self.require_ticket(id_or_key)?;
        let to = transitions::next(ticket.status, action, actor)
            .map_err(|error| anyhow!("{}: {error}", ticket.key))?;
        if action == TicketAction::Approve {
            let gaps = self.ready_gaps(&ticket)?;
            if !gaps.is_empty() {
                bail!(
                    "{} cannot be approved yet: it still needs {}.",
                    ticket.key,
                    gaps.join(" and ")
                );
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE board_tickets SET status = ?2, updated_at = ?3,
                 started_at = CASE WHEN ?4 THEN ?3 ELSE started_at END,
                 finished_at = CASE WHEN ?5 THEN ?3 ELSE finished_at END
             WHERE id = ?1 AND status = ?6",
            params![
                ticket.id,
                to.as_str(),
                now,
                action == TicketAction::Dispatch,
                matches!(
                    to,
                    TicketStatus::Review
                        | TicketStatus::Done
                        | TicketStatus::Rejected
                        | TicketStatus::Cancelled
                ) && ticket.finished_at.is_none(),
                ticket.status.as_str()
            ],
        )?;
        if changed == 0 {
            bail!("{} changed while this was being decided; look at it again with `orx ticket show {}`.", ticket.key, ticket.key);
        }
        Self::append_board_event(
            &tx,
            &ticket.project_id,
            &ticket.id,
            actor,
            action.name(),
            &json!({"from": ticket.status, "to": to, "detail": &detail}),
        )?;
        if to == TicketStatus::Blocked {
            Self::insert_decision(
                &tx,
                &ticket.project_id,
                ticket.epic_id.as_deref(),
                DecisionKind::Blocked,
                std::slice::from_ref(&ticket.id),
                &json!({"reason": detail_reason(&detail)}),
                &format!("blocked:{}:{}", ticket.id, uuid::Uuid::new_v4()),
            )?;
        }
        Self::settle_cards(&tx, &ticket.id, now)?;
        tx.commit()?;
        self.require_ticket(&ticket.id)
    }

    /// Puts `ticket_id` on the open card under `dedupe`, or opens one. A card
    /// that already closed keeps its key, so a later ticket gets a fresh card.
    fn add_to_card(
        conn: &Connection,
        project_id: &str,
        epic_id: Option<&str>,
        kind: DecisionKind,
        dedupe: &str,
        ticket_id: &str,
        payload: Value,
    ) -> Result<String> {
        let open: Option<(String, String, String)> = conn
            .query_row(
                "SELECT id, ticket_ids_json, payload_json FROM board_decisions
                 WHERE dedupe_key = ?1 AND status = 'open'",
                [dedupe],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((decision_id, ids, existing)) = open {
            let mut ids: Vec<String> = serde_json::from_str(&ids)?;
            if !ids.iter().any(|id| id == ticket_id) {
                ids.push(ticket_id.to_string());
            }
            let mut existing: Value = serde_json::from_str(&existing)?;
            merge_payload(&mut existing, payload);
            conn.execute(
                "UPDATE board_decisions SET ticket_ids_json = ?2, payload_json = ?3 WHERE id = ?1",
                params![
                    decision_id,
                    serde_json::to_string(&ids)?,
                    existing.to_string()
                ],
            )?;
            return Ok(decision_id);
        }
        let used = conn
            .query_row(
                "SELECT 1 FROM board_decisions WHERE dedupe_key = ?1",
                [dedupe],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let dedupe = if used {
            format!("{dedupe}:{}", uuid::Uuid::new_v4())
        } else {
            dedupe.to_string()
        };
        Self::insert_decision(
            conn,
            project_id,
            epic_id,
            kind,
            &[ticket_id.to_string()],
            &payload,
            &dedupe,
        )
    }

    /// Resolves open cards about `ticket_id` once none of their tickets still
    /// waits in the status the card decides on.
    fn settle_cards(conn: &Connection, ticket_id: &str, now: i64) -> Result<()> {
        let mut statement = conn.prepare(
            "SELECT d.id, d.kind, d.ticket_ids_json FROM board_decisions d, json_each(d.ticket_ids_json) j
             WHERE d.status = 'open' AND j.value = ?1",
        )?;
        let cards: Vec<(String, String, String)> = statement
            .query_map([ticket_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (card, kind, ids) in cards {
            let waiting_in = match DecisionKind::parse(&kind)? {
                DecisionKind::ApproveBatch => TicketStatus::Proposed,
                DecisionKind::AcceptBatch | DecisionKind::AcceptOne => TicketStatus::Review,
                DecisionKind::Question => TicketStatus::Waiting,
                DecisionKind::Blocked => TicketStatus::Blocked,
                _ => continue,
            };
            let ids: Vec<String> = serde_json::from_str(&ids)?;
            let mut outcome = serde_json::Map::new();
            let mut pending = false;
            for id in &ids {
                let status: String = conn.query_row(
                    "SELECT status FROM board_tickets WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )?;
                pending |= status == waiting_in.as_str();
                outcome.insert(id.clone(), Value::String(status));
            }
            if !pending {
                conn.execute(
                    "UPDATE board_decisions SET status = 'resolved', resolved_at = ?2,
                         resolution_json = COALESCE(resolution_json, ?3)
                     WHERE id = ?1",
                    params![card, now, json!({"tickets": outcome}).to_string()],
                )?;
            }
        }
        Ok(())
    }

    /// The executor hands its result in. The board checks it against the
    /// epic's criteria and puts it on the batch acceptance card when it
    /// passes, or on a card of its own with the reasons when it does not.
    pub fn report_ticket(
        &self,
        id_or_key: &str,
        outcome: Outcome,
        conclusion: &str,
        result: Option<&Value>,
        actor: &Actor,
    ) -> Result<Ticket> {
        if conclusion.trim().is_empty() {
            bail!("A report needs a conclusion: one or two sentences on what the result shows.");
        }
        let before = self.require_ticket(id_or_key)?;
        if before.kind == TicketKind::Experiment && result.is_none() {
            bail!("{} is an experiment: report its numbers with `--result-file` (metric, per-seed values, baseline) so they can be checked.", before.key);
        }
        let ticket = self.move_ticket(
            id_or_key,
            TicketAction::Report,
            actor,
            json!({"outcome": outcome}),
        )?;
        self.conn.execute(
            "UPDATE board_tickets SET outcome = ?2, conclusion = ?3, result_json = COALESCE(?4, result_json)
             WHERE id = ?1",
            params![ticket.id, outcome.as_str(), conclusion.trim(), result.map(Value::to_string)],
        )?;
        self.route_review(&self.require_ticket(&ticket.id)?)
    }

    fn route_review(&self, ticket: &Ticket) -> Result<Ticket> {
        let outcome = ticket
            .outcome
            .ok_or_else(|| anyhow!("{} reached review without an outcome", ticket.key))?;
        let epic = ticket
            .epic_id
            .as_deref()
            .map(|id| self.require_epic(id))
            .transpose()?;
        let rules = epic
            .as_ref()
            .map(|epic| epic.rules.clone())
            .unwrap_or_default();
        let overrides = match &epic {
            Some(epic) if epic.criteria_version > 0 => self
                .criteria(&epic.id, epic.criteria_version)?
                .map(|criteria| criteria.checks)
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };
        let checks = Checks::merged(&self.board_setting("checks")?, &overrides);
        let verdict = match (&ticket.kind, &ticket.result) {
            (TicketKind::Experiment, Some(result)) => {
                Some(checks::evaluate(&checks, result, outcome))
            }
            _ => None,
        };
        let passed = verdict.as_ref().is_some_and(|verdict| verdict.passed);
        let batchable = passed
            && (outcome != Outcome::Refutes
                || (rules.negative_in_batch && !ticket.core_hypothesis));
        let tx = self.conn.unchecked_transaction()?;
        Self::append_board_event(
            &tx,
            &ticket.project_id,
            &ticket.id,
            &Actor::System(None),
            "checked",
            &json!({"verdict": verdict, "batchable": batchable}),
        )?;
        let scope = ticket
            .epic_id
            .clone()
            .unwrap_or_else(|| ticket.project_id.clone());
        let payload = json!({"verdicts": {ticket.id.clone(): verdict}});
        if !(batchable && rules.auto_done) {
            if batchable {
                Self::add_to_card(
                    &tx,
                    &ticket.project_id,
                    ticket.epic_id.as_deref(),
                    DecisionKind::AcceptBatch,
                    &format!("accept:{scope}"),
                    &ticket.id,
                    payload,
                )?;
            } else {
                Self::insert_decision(
                    &tx,
                    &ticket.project_id,
                    ticket.epic_id.as_deref(),
                    DecisionKind::AcceptOne,
                    std::slice::from_ref(&ticket.id),
                    &payload,
                    &format!("accept-one:{}:{}", ticket.id, uuid::Uuid::new_v4()),
                )?;
            }
        }
        tx.commit()?;
        if batchable && rules.auto_done {
            return self.move_ticket(
                &ticket.id,
                TicketAction::Accept,
                &Actor::System(Some("auto_done".into())),
                json!({"verdict": verdict}),
            );
        }
        self.require_ticket(&ticket.id)
    }

    /// A global board setting, as JSON; `null` when unset.
    pub fn board_setting(&self, key: &str) -> Result<Value> {
        let text: Option<String> = self
            .conn
            .query_row("SELECT v FROM board_settings WHERE k = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        text.map(|text| {
            serde_json::from_str(&text)
                .with_context(|| format!("board setting `{key}` is not JSON"))
        })
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
    }

    pub fn set_board_setting(&self, key: &str, value: &Value, actor: &Actor) -> Result<()> {
        require_user(actor, "change board settings")?;
        self.conn.execute(
            "INSERT INTO board_settings (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    /// What the last phone notification covered.
    pub fn board_pushed(&self) -> Result<crate::board::notify::Pushed> {
        Ok(serde_json::from_value(self.board_setting("ntfyPushed")?).unwrap_or_default())
    }

    pub fn note_board_pushed(&self, pushed: crate::board::notify::Pushed) -> Result<()> {
        self.conn.execute(
            "INSERT INTO board_settings (k, v) VALUES ('ntfyPushed', ?1) ON CONFLICT(k) DO UPDATE SET v = ?1",
            [serde_json::to_string(&pushed)?],
        )?;
        Ok(())
    }

    /// The executor asks the user something; only this ticket waits for the answer.
    pub fn ask_on_ticket(
        &self,
        id_or_key: &str,
        question: &str,
        options: &[String],
        recommend: Option<&str>,
        actor: &Actor,
    ) -> Result<Decision> {
        if question.trim().is_empty() {
            bail!("Ask a question: `--question \"…\"`.");
        }
        if !options.is_empty() && !(2..=4).contains(&options.len()) {
            bail!("Offer 2 to 4 options so the user can answer with one tap, or none for a free-text answer.");
        }
        if let Some(recommend) = recommend {
            if !options.iter().any(|option| option == recommend) {
                bail!("The recommended option `{recommend}` is not one of the options.");
            }
        }
        let ticket = self.move_ticket(
            id_or_key,
            TicketAction::Ask,
            actor,
            json!({"question": question}),
        )?;
        let id = Self::insert_decision(
            &self.conn,
            &ticket.project_id,
            ticket.epic_id.as_deref(),
            DecisionKind::Question,
            std::slice::from_ref(&ticket.id),
            &json!({"question": question.trim(), "options": options, "recommend": recommend}),
            &format!("question:{}:{}", ticket.id, uuid::Uuid::new_v4()),
        )?;
        self.require_decision(&id)
    }

    pub fn comment_on_ticket(&self, id_or_key: &str, text: &str, actor: &Actor) -> Result<()> {
        let ticket = self.require_ticket(id_or_key)?;
        if text.trim().is_empty() {
            bail!("The comment is empty.");
        }
        Self::append_board_event(
            &self.conn,
            &ticket.project_id,
            &ticket.id,
            actor,
            "comment",
            &json!({"text": text.trim()}),
        )
    }

    pub fn taint_ticket(&self, id_or_key: &str, reason: &str, actor: &Actor) -> Result<Ticket> {
        require_user(actor, "mark a result as tainted")?;
        let ticket = self.require_ticket(id_or_key)?;
        if ticket.status != TicketStatus::Done {
            bail!(
                "Only a done ticket's result can be tainted; {} is {}.",
                ticket.key,
                ticket.status
            );
        }
        if reason.trim().is_empty() {
            bail!("Say why the result can no longer be trusted: `--reason \"…\"`.");
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE board_tickets SET tainted = 1, tainted_reason = ?2, updated_at = ?3 WHERE id = ?1",
            params![ticket.id, reason.trim(), now_ms()],
        )?;
        Self::append_board_event(
            &tx,
            &ticket.project_id,
            &ticket.id,
            actor,
            "taint",
            &json!({"reason": reason.trim()}),
        )?;
        tx.commit()?;
        self.require_ticket(&ticket.id)
    }

    fn insert_decision(
        conn: &Connection,
        project_id: &str,
        epic_id: Option<&str>,
        kind: DecisionKind,
        ticket_ids: &[String],
        payload: &Value,
        dedupe_key: &str,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let inserted = conn.execute(
            "INSERT INTO board_decisions (id, project_id, epic_id, kind, ticket_ids_json,
                 payload_json, status, dedupe_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8)
             ON CONFLICT(dedupe_key) DO NOTHING",
            params![
                id,
                project_id,
                epic_id,
                kind.as_str(),
                serde_json::to_string(ticket_ids)?,
                payload.to_string(),
                dedupe_key,
                now_ms()
            ],
        )?;
        if inserted == 1 {
            return Ok(id);
        }
        Ok(conn.query_row(
            "SELECT id FROM board_decisions WHERE dedupe_key = ?1",
            [dedupe_key],
            |row| row.get(0),
        )?)
    }

    pub fn decision(&self, id: &str) -> Result<Option<Decision>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {DECISION_COLUMNS} FROM board_decisions WHERE id = ?1"),
                [id],
                decision_from_row,
            )
            .optional()?)
    }

    pub fn require_decision(&self, id: &str) -> Result<Decision> {
        self.decision(id)?
            .ok_or_else(|| anyhow!("No decision `{id}`. Open ones are listed by `orx board`."))
    }

    /// Open decisions, oldest first, for one project or all of them.
    pub fn open_decisions(&self, project_id: Option<&str>) -> Result<Vec<Decision>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {DECISION_COLUMNS} FROM board_decisions
             WHERE status = 'open' AND (?1 IS NULL OR project_id = ?1) ORDER BY created_at"
        ))?;
        let decisions = statement
            .query_map([project_id], decision_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(decisions)
    }

    /// Applies the user's answer to a decision card.
    pub fn resolve_decision(
        &self,
        id: &str,
        choice: &DecisionChoice,
        actor: &Actor,
    ) -> Result<Decision> {
        require_user(actor, "decide")?;
        let decision = self.require_decision(id)?;
        if decision.status != DecisionStatus::Open {
            bail!("This decision is already {}.", decision.status);
        }
        match decision.kind {
            DecisionKind::ApproveBatch => {
                let selected = self.card_selection(&decision, choice)?;
                // Check every selected ticket first so one gap does not leave half a batch approved.
                for ticket_id in &selected {
                    let ticket = self.require_ticket(ticket_id)?;
                    let gaps = self.ready_gaps(&ticket)?;
                    if ticket.status == TicketStatus::Proposed && !gaps.is_empty() {
                        bail!(
                            "{} cannot be approved yet: it still needs {}.",
                            ticket.key,
                            gaps.join(" and ")
                        );
                    }
                }
                for ticket_id in &decision.ticket_ids {
                    let ticket = self.require_ticket(ticket_id)?;
                    if ticket.status != TicketStatus::Proposed {
                        continue;
                    }
                    let action = if selected.contains(ticket_id) {
                        TicketAction::Approve
                    } else {
                        TicketAction::Reject
                    };
                    self.move_ticket(
                        ticket_id,
                        action,
                        actor,
                        json!({"decision": decision.id, "note": choice.text}),
                    )?;
                }
            }
            DecisionKind::Question => {
                let answer = choice
                    .option
                    .clone()
                    .or_else(|| choice.text.clone())
                    .ok_or_else(|| {
                        anyhow!("Answer with `--option <one of the options>` or `--text \"…\"`.")
                    })?;
                if let (Some(option), Some(options)) =
                    (&choice.option, decision.payload["options"].as_array())
                {
                    if !options.is_empty()
                        && !options.iter().any(|known| known.as_str() == Some(option))
                    {
                        bail!("`{option}` is not one of this question's options.");
                    }
                }
                for ticket_id in &decision.ticket_ids {
                    self.move_ticket(
                        ticket_id,
                        TicketAction::Answer,
                        actor,
                        json!({"decision": decision.id, "answer": answer}),
                    )?;
                }
            }
            DecisionKind::AcceptBatch => {
                let selected = self.card_selection(&decision, choice)?;
                for ticket_id in &decision.ticket_ids {
                    let ticket = self.require_ticket(ticket_id)?;
                    if ticket.status != TicketStatus::Review {
                        continue;
                    }
                    if selected.contains(ticket_id) {
                        self.move_ticket(
                            ticket_id,
                            TicketAction::Accept,
                            actor,
                            json!({"decision": decision.id}),
                        )?;
                    } else {
                        // Unticked results get a card of their own to accept or dispute.
                        Self::insert_decision(
                            &self.conn,
                            &ticket.project_id,
                            ticket.epic_id.as_deref(),
                            DecisionKind::AcceptOne,
                            std::slice::from_ref(ticket_id),
                            &json!({"verdicts": {ticket_id.clone(): decision.payload["verdicts"][ticket_id]}, "unticked": true}),
                            &format!("accept-one:{ticket_id}:{}", uuid::Uuid::new_v4()),
                        )?;
                    }
                }
            }
            DecisionKind::AcceptOne => {
                let (action, detail) = match choice.option.as_deref() {
                    Some("accept") => (TicketAction::Accept, json!({"decision": decision.id})),
                    Some("challenge") => {
                        let note = choice
                            .text
                            .clone()
                            .filter(|text| !text.trim().is_empty())
                            .ok_or_else(|| anyhow!("Say what to look at again: `--text \"…\"`."))?;
                        (
                            TicketAction::Challenge,
                            json!({"decision": decision.id, "note": note}),
                        )
                    }
                    _ => bail!("Choose `--option accept` or `--option challenge --text \"…\"`."),
                };
                for ticket_id in &decision.ticket_ids {
                    self.move_ticket(ticket_id, action, actor, detail.clone())?;
                }
            }
            DecisionKind::Blocked => {
                let action = match choice.option.as_deref() {
                    Some("unblock") => TicketAction::Unblock,
                    Some("reject") => TicketAction::Reject,
                    _ => bail!(
                        "Choose `--option unblock` to send it back to ready, or `--option reject`."
                    ),
                };
                for ticket_id in &decision.ticket_ids {
                    self.move_ticket(
                        ticket_id,
                        action,
                        actor,
                        json!({"decision": decision.id, "note": choice.text}),
                    )?;
                }
            }
            DecisionKind::ApproveCriteria => {
                let version = decision.payload["version"]
                    .as_i64()
                    .ok_or_else(|| anyhow!("criteria decision {} has no version", decision.id))?;
                let epic = decision
                    .epic_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("criteria decision {} has no epic", decision.id))?;
                self.approve_criteria(epic, version, actor)?;
            }
            // A budget card informs; seeing it is the decision. Topping up is a change to the epic.
            DecisionKind::Budget => {}
            DecisionKind::Import => match choice.option.as_deref() {
                Some("import") | None => self.apply_import(&decision, choice)?,
                Some("discard") => {}
                Some(other) => {
                    bail!("An import card is answered with `import` or `discard`, not `{other}`.")
                }
            },
            DecisionKind::PublishGate => match choice.option.as_deref() {
                Some("allow" | "deny") => {}
                _ => bail!(
                    "A publish card is answered with `--option allow` (once) or `--option deny`."
                ),
            },
            other => bail!("`{other}` cards cannot be decided yet."),
        }
        self.record_choice(&decision, choice)?;
        self.require_decision(&decision.id)
    }

    /// The tickets a batch card's choice selects: all of them unless `--select` narrowed it.
    fn card_selection(&self, decision: &Decision, choice: &DecisionChoice) -> Result<Vec<String>> {
        let selected = choice
            .selected
            .clone()
            .unwrap_or_else(|| decision.ticket_ids.clone());
        let selected: Vec<String> = selected
            .iter()
            .map(|key| self.require_ticket(key).map(|ticket| ticket.id))
            .collect::<Result<_>>()?;
        if let Some(stray) = selected.iter().find(|id| !decision.ticket_ids.contains(id)) {
            bail!("Ticket {stray} is not on this card.");
        }
        Ok(selected)
    }

    /// Stores what the user chose; moving the tickets may already have settled the card.
    fn record_choice(&self, decision: &Decision, choice: &DecisionChoice) -> Result<()> {
        self.conn.execute(
            "UPDATE board_decisions SET status = 'resolved', resolved_at = COALESCE(resolved_at, ?2),
                 resolution_json = ?3
             WHERE id = ?1",
            params![
                decision.id,
                now_ms(),
                json!({"selected": choice.selected, "option": choice.option, "text": choice.text, "groups": choice.groups})
                    .to_string()
            ],
        )?;
        Ok(())
    }

    /// Whether the board has work in flight or waiting to start: `orx up` must stay up for it.
    pub fn board_has_live_work(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM board_tickets t JOIN board_projects p ON p.project_id = t.project_id
                           WHERE p.enabled = 1 AND t.status IN ('ready', 'running'))",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn tickets_in(&self, statuses: &[TicketStatus]) -> Result<Vec<Ticket>> {
        let words: Vec<&str> = statuses.iter().map(|status| status.as_str()).collect();
        let mut statement = self.conn.prepare(&format!(
            "SELECT {TICKET_COLUMNS} FROM board_tickets
             WHERE status IN (SELECT value FROM json_each(?1)) ORDER BY created_at"
        ))?;
        let tickets = statement
            .query_map([serde_json::to_string(&words)?], ticket_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(tickets)
    }

    /// GPU hours an epic's experiments have used: each run's time times its ticket's GPUs.
    pub fn epic_gpu_hours(&self, epic_id: &str) -> Result<f64> {
        let now = now_ms();
        let mut statement = self.conn.prepare(
            "SELECT r.created_at, COALESCE(r.ended_at, ?2), t.needs_gpus
             FROM runs r JOIN board_tickets t ON t.experiment_id = r.experiment_id
             WHERE t.epic_id = ?1",
        )?;
        let runs: Vec<(i64, i64, i64)> = statement
            .query_map(params![epic_id, now], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(scheduler::gpu_hours(&runs))
    }

    /// Ready tickets on enabled boards whose epic is active (or that have no epic).
    pub fn dispatch_candidates(&self) -> Result<Vec<Candidate>> {
        let now = now_ms();
        let mut statement = self.conn.prepare(
            "SELECT t.id, p.priority, COALESCE(e.priority, 100), t.priority, t.created_at,
                    t.needs_gpus, p.confidential, t.epic_id, e.budget_gpu_hours, e.budget_deadline_ms,
                    EXISTS(SELECT 1 FROM board_relations r JOIN board_tickets d ON d.id = r.to_id
                           WHERE r.from_id = t.id AND r.kind = 'blocked_by' AND d.status != 'done')
             FROM board_tickets t
             JOIN board_projects p ON p.project_id = t.project_id AND p.enabled = 1
             LEFT JOIN board_epics e ON e.id = t.epic_id
             WHERE t.status = 'ready' AND (t.epic_id IS NULL OR e.status = 'active')",
        )?;
        // A candidate with its epic's id, GPU-hour budget and deadline.
        type Row = (Candidate, Option<String>, Option<f64>, Option<i64>);
        let rows: Vec<Row> = statement
            .query_map([], |row| {
                Ok((
                    Candidate {
                        ticket_id: row.get(0)?,
                        project_priority: row.get(1)?,
                        epic_priority: row.get(2)?,
                        priority: row.get(3)?,
                        created_at: row.get(4)?,
                        needs_gpus: row.get(5)?,
                        confidential: row.get(6)?,
                        waiting_on_dependency: row.get(10)?,
                        out_of_budget: false,
                    },
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(mut candidate, epic, budget, deadline)| {
                let spent = match (&epic, budget) {
                    (Some(epic), Some(_)) => self.epic_gpu_hours(epic)?,
                    _ => 0.0,
                };
                candidate.out_of_budget = scheduler::budget_marks(spent, budget).contains(&100)
                    || deadline.is_some_and(|deadline| deadline <= now);
                Ok(candidate)
            })
            .collect()
    }

    /// GPU slots per machine. A ticket waiting on the user holds its slot only
    /// while one of its runs is still going, so a question never idles a GPU.
    pub fn machine_slots(&self) -> Result<Vec<MachineSlots>> {
        let mut statement = self.conn.prepare(
            "SELECT m.id, m.gpu_slots, m.own,
                    COALESCE((SELECT SUM(t.needs_gpus) FROM board_tickets t
                              WHERE t.machine_id = m.id AND (t.status = 'running'
                                 OR (t.status = 'waiting' AND EXISTS(
                                     SELECT 1 FROM runs r WHERE r.experiment_id = t.experiment_id
                                     AND r.ended_at IS NULL)))), 0)
             FROM board_machines m WHERE m.enabled = 1",
        )?;
        let slots = statement
            .query_map([], |row| {
                Ok(MachineSlots {
                    machine_id: row.get(0)?,
                    gpu_slots: row.get(1)?,
                    own: row.get(2)?,
                    in_use: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(slots)
    }

    pub fn machine(&self, id: &str) -> Result<Option<Machine>> {
        Ok(self
            .list_machines()?
            .into_iter()
            .find(|machine| machine.id == id))
    }

    /// Starts a ready ticket on `machine_id` for the scheduler.
    pub fn dispatch_ticket(&self, ticket_id: &str, machine_id: Option<&str>) -> Result<Ticket> {
        let ticket = self.move_ticket(
            ticket_id,
            TicketAction::Dispatch,
            &Actor::System(None),
            json!({"machine": machine_id}),
        )?;
        self.conn.execute(
            "UPDATE board_tickets SET machine_id = ?2, slot_lease_at = ?3 WHERE id = ?1",
            params![ticket.id, machine_id, now_ms()],
        )?;
        self.require_ticket(&ticket.id)
    }

    pub fn bind_ticket_session(&self, ticket_id: &str, session_id: &str) -> Result<()> {
        let ticket = self.require_ticket(ticket_id)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE board_tickets SET session_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![ticket.id, session_id, now_ms()],
        )?;
        Self::append_board_event(
            &tx,
            &ticket.project_id,
            &ticket.id,
            &Actor::System(None),
            "session",
            &json!({"sessionId": session_id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The scheduler gives a dispatched ticket back after a failed start,
    /// blocking it once it has failed `repair_cap` times.
    pub fn requeue_ticket(&self, ticket_id: &str, reason: &str) -> Result<Ticket> {
        let ticket = self.require_ticket(ticket_id)?;
        let cap = ticket
            .epic_id
            .as_deref()
            .map(|epic| self.require_epic(epic).map(|epic| epic.rules.repair_cap))
            .transpose()?
            .unwrap_or(EpicRules::default().repair_cap);
        self.conn.execute(
            "UPDATE board_tickets SET repair_count = repair_count + 1, session_id = NULL,
                 machine_id = NULL, slot_lease_at = NULL WHERE id = ?1",
            [&ticket.id],
        )?;
        let action = if ticket.repair_count + 1 >= i64::from(cap) {
            TicketAction::Block
        } else {
            TicketAction::Requeue
        };
        self.move_ticket(
            &ticket.id,
            action,
            &Actor::System(None),
            json!({"reason": reason}),
        )
    }

    pub fn ticket_for_session(&self, session_id: &str) -> Result<Option<Ticket>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {TICKET_COLUMNS} FROM board_tickets WHERE session_id = ?1
                          ORDER BY updated_at DESC LIMIT 1"
                ),
                [session_id],
                ticket_from_row,
            )
            .optional()?)
    }

    pub fn ticket_for_experiment(&self, experiment_id: &str) -> Result<Option<Ticket>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {TICKET_COLUMNS} FROM board_tickets WHERE experiment_id = ?1"),
                [experiment_id],
                ticket_from_row,
            )
            .optional()?)
    }

    /// Ties the first experiment a ticket's chat creates to that ticket.
    pub fn link_ticket_experiment(&self, session_id: &str, experiment_id: &str) -> Result<()> {
        let Some(ticket) = self.ticket_for_session(session_id)? else {
            return Ok(());
        };
        if ticket.kind != TicketKind::Experiment
            || ticket.experiment_id.is_some()
            || !matches!(ticket.status, TicketStatus::Running | TicketStatus::Waiting)
        {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE board_tickets SET experiment_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![ticket.id, experiment_id, now_ms()],
        )?;
        Self::append_board_event(
            &tx,
            &ticket.project_id,
            &ticket.id,
            &Actor::Agent(session_id.into()),
            "experiment",
            &json!({"experimentId": experiment_id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Logs that one of a ticket's runs ended. False when this run was already logged.
    pub fn record_ticket_run_end(
        &self,
        ticket: &Ticket,
        run_id: &str,
        phase: &str,
    ) -> Result<bool> {
        let seen: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM board_events WHERE subject_id = ?1 AND kind = 'run_ended'
                           AND json_extract(detail_json, '$.runId') = ?2)",
            params![ticket.id, run_id],
            |row| row.get(0),
        )?;
        if seen {
            return Ok(false);
        }
        Self::append_board_event(
            &self.conn,
            &ticket.project_id,
            &ticket.id,
            &Actor::System(None),
            "run_ended",
            &json!({"runId": run_id, "phase": phase}),
        )?;
        self.conn.execute(
            "UPDATE board_tickets SET updated_at = ?2 WHERE id = ?1",
            params![ticket.id, now_ms()],
        )?;
        Ok(true)
    }

    /// Logs `kind` on a ticket once; false when it was already logged.
    pub fn note_ticket_once(&self, ticket: &Ticket, kind: &str, detail: &Value) -> Result<bool> {
        let seen: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM board_events WHERE subject_id = ?1 AND kind = ?2)",
            params![ticket.id, kind],
            |row| row.get(0),
        )?;
        if !seen {
            Self::append_board_event(
                &self.conn,
                &ticket.project_id,
                &ticket.id,
                &Actor::System(None),
                kind,
                detail,
            )?;
        }
        Ok(!seen)
    }

    /// Opens a budget card when an epic crosses 80 or 100 percent of its GPU hours.
    pub fn check_epic_budget(&self, epic: &Epic) -> Result<()> {
        let spent = self.epic_gpu_hours(&epic.id)?;
        for mark in scheduler::budget_marks(spent, epic.budget_gpu_hours) {
            Self::insert_decision(
                &self.conn,
                &epic.project_id,
                Some(&epic.id),
                DecisionKind::Budget,
                &[],
                &json!({"mark": mark, "spentGpuHours": spent, "budgetGpuHours": epic.budget_gpu_hours}),
                &format!(
                    "budget:{}:{mark}:{}",
                    epic.id,
                    epic.budget_gpu_hours.unwrap_or(0.0)
                ),
            )?;
        }
        Ok(())
    }

    pub fn active_epics(&self) -> Result<Vec<Epic>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {EPIC_COLUMNS} FROM board_epics WHERE status = 'active' ORDER BY priority"
        ))?;
        let epics = statement
            .query_map([], epic_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(epics)
    }

    /// The user's answers, challenges and comments that have not reached the
    /// ticket's chat yet, oldest first.
    pub fn undelivered_ticket_messages(&self) -> Result<Vec<(BoardEvent, Ticket)>> {
        let mut statement = self.conn.prepare(
            "SELECT e.id FROM board_events e JOIN board_tickets t ON t.id = e.subject_id
             WHERE e.actor = 'user' AND e.kind IN ('answer', 'challenge', 'comment')
               AND t.session_id IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM board_events d WHERE d.subject_id = e.subject_id
                              AND d.kind = 'delivered' AND json_extract(d.detail_json, '$.eventId') = e.id)
             ORDER BY e.id",
        )?;
        let ids: Vec<i64> = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| {
                let event = self.conn.query_row(
                    "SELECT id, at, project_id, subject_id, actor, kind, detail_json FROM board_events WHERE id = ?1",
                    [id],
                    |row| {
                        Ok(BoardEvent {
                            id: row.get(0)?,
                            at: row.get(1)?,
                            project_id: row.get(2)?,
                            subject_id: row.get(3)?,
                            actor: row.get(4)?,
                            kind: row.get(5)?,
                            detail: json_column(row, 6)?.unwrap_or(Value::Null),
                        })
                    },
                )?;
                let ticket = self.require_ticket(&event.subject_id)?;
                Ok((event, ticket))
            })
            .collect()
    }

    /// Runs of a ticket that have not ended: its experiment's, and any its chat launched.
    pub fn ticket_live_runs(&self, ticket: &Ticket) -> Result<Vec<super::StoredRun>> {
        let mut runs = match &ticket.experiment_id {
            Some(experiment) => self.list_runs_by_experiment(experiment)?,
            None => Vec::new(),
        };
        if let Some(session) = &ticket.session_id {
            for run in self.list_runs_by_project(&ticket.project_id)? {
                if run.chat_session_id.as_ref() == Some(session)
                    && !runs.iter().any(|seen| seen.id == run.id)
                {
                    runs.push(run);
                }
            }
        }
        runs.retain(|run| {
            !crate::engine::Phase::parse(&run.status).is_some_and(crate::engine::Phase::is_terminal)
        });
        Ok(runs)
    }

    /// Cancelled or rejected tickets whose chat has not been stopped yet.
    pub fn tickets_to_stop(&self) -> Result<Vec<Ticket>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {TICKET_COLUMNS} FROM board_tickets t
             WHERE t.status IN ('cancelled', 'rejected') AND t.session_id IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM board_events e WHERE e.subject_id = t.id AND e.kind = 'chat_stopped')"
        ))?;
        let tickets = statement
            .query_map([], ticket_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(tickets)
    }

    pub fn mark_ticket_message_delivered(
        &self,
        ticket: &Ticket,
        event_id: i64,
        session_id: &str,
    ) -> Result<()> {
        Self::append_board_event(
            &self.conn,
            &ticket.project_id,
            &ticket.id,
            &Actor::System(None),
            "delivered",
            &json!({"eventId": event_id, "sessionId": session_id}),
        )
    }

    /// Changes whenever any board record does, for the dashboard's event stream.
    pub fn board_change_marker(&self) -> Result<String> {
        Ok(self.conn.query_row(
            "SELECT COALESCE((SELECT MAX(id) FROM board_events), 0) || ':' ||
                    COALESCE((SELECT MAX(updated_at) FROM board_tickets), 0) || ':' ||
                    COALESCE((SELECT MAX(updated_at) FROM board_epics), 0) || ':' ||
                    COALESCE((SELECT MAX(MAX(created_at, COALESCE(resolved_at, 0))) FROM board_decisions), 0) || ':' ||
                    COALESCE((SELECT MAX(updated_at) FROM board_machines), 0) || ':' ||
                    COALESCE((SELECT MAX(updated_at) FROM board_projects), 0) || ':' ||
                    (SELECT COUNT(*) FROM board_machines)",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn board_events(&self, subject_id: &str) -> Result<Vec<BoardEvent>> {
        let mut statement = self.conn.prepare(
            "SELECT id, at, project_id, subject_id, actor, kind, detail_json FROM board_events
             WHERE subject_id = ?1 ORDER BY id",
        )?;
        let events = statement
            .query_map([subject_id], |row| {
                Ok(BoardEvent {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    project_id: row.get(2)?,
                    subject_id: row.get(3)?,
                    actor: row.get(4)?,
                    kind: row.get(5)?,
                    detail: json_column(row, 6)?.unwrap_or(Value::Null),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(events)
    }

    pub fn upsert_machine(
        &self,
        name: &str,
        backend: &str,
        gpu_slots: i64,
        own: bool,
    ) -> Result<Machine> {
        let backend = backend.trim();
        if backend != "local"
            && !backend
                .strip_prefix("ssh:")
                .is_some_and(|alias| !alias.is_empty())
        {
            bail!("A machine's backend is `local` or `ssh:<alias>`; `{backend}` is neither.");
        }
        if gpu_slots < 0 {
            bail!("GPU slots cannot be negative.");
        }
        if name.trim().is_empty() {
            bail!("Name the machine.");
        }
        self.conn.execute(
            "INSERT INTO board_machines (id, name, backend, gpu_slots, own, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET backend = ?3, gpu_slots = ?4, own = ?5, updated_at = ?6",
            params![uuid::Uuid::new_v4().to_string(), name.trim(), backend, gpu_slots, own, now_ms()],
        )?;
        self.list_machines()?
            .into_iter()
            .find(|machine| machine.name == name.trim())
            .ok_or_else(|| anyhow!("machine {name} was not saved"))
    }

    pub fn remove_machine(&self, name: &str) -> Result<()> {
        let busy: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM board_tickets t JOIN board_machines m ON m.id = t.machine_id
             WHERE m.name = ?1 AND t.status IN ('running', 'waiting')",
            [name],
            |row| row.get(0),
        )?;
        if busy > 0 {
            bail!("{busy} running ticket(s) hold slots on {name}; wait for them or cancel them first.");
        }
        if self
            .conn
            .execute("DELETE FROM board_machines WHERE name = ?1", [name])?
            == 0
        {
            bail!("No machine named `{name}`. List them with `orx board machines`.");
        }
        Ok(())
    }

    pub fn list_machines(&self) -> Result<Vec<Machine>> {
        let mut statement = self.conn.prepare(
            "SELECT id, name, backend, gpu_slots, own, enabled, updated_at FROM board_machines ORDER BY name",
        )?;
        let machines = statement
            .query_map([], |row| {
                Ok(Machine {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    backend: row.get(2)?,
                    gpu_slots: row.get(3)?,
                    own: row.get(4)?,
                    enabled: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(machines)
    }

    /// Whether the user, not orx or another chat, wrote the message that
    /// started the chat's latest turn.
    pub fn human_started_turn(&self, session_id: &str) -> Result<bool> {
        let Some(turn) = self.latest_chat_turn(session_id)? else {
            return Ok(false);
        };
        let Some(message) = turn
            .user_message_id
            .as_deref()
            .map(|id| self.get_chat_message(id))
            .transpose()?
            .flatten()
        else {
            return Ok(false);
        };
        let parts: Vec<Value> = serde_json::from_str(&message.parts_json).unwrap_or_default();
        Ok(message.role == "user"
            && parts
                .iter()
                .any(|part| part["type"] == "text" && part.get("orx").is_none()))
    }

    /// Lets a chat create an experiment in `project_id` only for a ticket it
    /// works, or when the user asked for it in this turn, which opens an
    /// unplanned ticket. Without a chat the user is acting directly.
    pub fn gate_experiment_create(
        &self,
        project_id: &str,
        session_id: Option<&str>,
        title: Option<&str>,
    ) -> Result<()> {
        let Some(session_id) = session_id else {
            return Ok(());
        };
        if !self
            .board_project(project_id)?
            .is_some_and(|board| board.enabled)
        {
            return Ok(());
        }
        let bound = self
            .ticket_for_session(session_id)?
            .filter(|ticket| ticket.project_id == project_id);
        let human = self.human_started_turn(session_id)?;
        match gate::create_verdict(bound.as_ref(), human) {
            gate::CreateVerdict::Allow => Ok(()),
            gate::CreateVerdict::Unplanned => {
                self.start_unplanned_ticket(project_id, session_id, title)?;
                Ok(())
            }
            gate::CreateVerdict::Deny(why) => bail!("{why}"),
        }
    }

    /// A ticket for an experiment the user asked for in their own chat: it is
    /// approved by the asking, so it starts running in that chat.
    pub fn start_unplanned_ticket(
        &self,
        project_id: &str,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<Ticket> {
        let board = self.require_board_project(project_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let seq: i64 = tx.query_row(
            "UPDATE board_projects SET next_ticket = next_ticket + 1 WHERE project_id = ?1
             RETURNING next_ticket - 1",
            [project_id],
            |row| row.get(0),
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!("{}-{seq:03}", board.key_prefix);
        let now = now_ms();
        let title = title
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or("Experiment asked for in chat");
        tx.execute(
            "INSERT INTO board_tickets (id, project_id, key, kind, title, status, session_id,
                 proposed_by, started_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'experiment', ?4, 'running', ?5, ?6, ?7, ?7, ?7)",
            params![
                id,
                project_id,
                key,
                title,
                session_id,
                ProposedBy::UserChat.as_str(),
                now
            ],
        )?;
        Self::append_board_event(
            &tx,
            project_id,
            &id,
            &Actor::User,
            "unplanned",
            &json!({"key": key, "sessionId": session_id}),
        )?;
        tx.commit()?;
        self.require_ticket(&id)
    }

    /// Refuses a run of `experiment_id` on a machine its ticket or its
    /// project's confidentiality does not allow.
    pub fn gate_launch(
        &self,
        experiment_id: &str,
        session_id: Option<&str>,
        backend: &str,
        host: Option<&str>,
    ) -> Result<()> {
        let Some(experiment) = self.get_local_experiment(experiment_id)? else {
            return Ok(());
        };
        let Some(board) = self
            .board_project(&experiment.project_id)?
            .filter(|board| board.enabled)
        else {
            return Ok(());
        };
        let ticket = match self.ticket_for_experiment(experiment_id)? {
            Some(ticket) => Some(ticket),
            None => session_id
                .map(|session| self.ticket_for_session(session))
                .transpose()?
                .flatten(),
        }
        .filter(|ticket| matches!(ticket.status, TicketStatus::Running | TicketStatus::Waiting));
        let machine = ticket
            .as_ref()
            .and_then(|ticket| ticket.machine_id.as_deref())
            .map(|id| self.machine(id))
            .transpose()?
            .flatten();
        gate::backend_verdict(
            board.confidential,
            ticket.as_ref().zip(machine.as_ref()),
            &self.list_machines()?,
            &gate::launch_target(backend, host),
        )
        .map_err(|why| anyhow!("{why}"))
    }

    /// How many of the project's experiments no ticket holds.
    pub fn unticketed_experiments(&self, project_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM local_experiments e WHERE e.project_id = ?1
               AND NOT EXISTS(SELECT 1 FROM board_tickets t WHERE t.experiment_id = e.id)",
            [project_id],
            |row| row.get(0),
        )?)
    }

    /// Drafts epics for the project's experiments that no ticket holds yet and
    /// puts them on one import card; nothing is written until the user confirms
    /// it. `groups` replaces the drafted grouping (from a file the user or an
    /// agent wrote); experiments it leaves out go to the unsorted group.
    pub fn propose_import(
        &self,
        project_id: &str,
        groups: Option<Vec<import::Group>>,
        actor: &Actor,
    ) -> Result<Decision> {
        self.require_board_project(project_id)?;
        let mut nodes = Vec::new();
        let mut shown = serde_json::Map::new();
        for experiment in self.list_experiments_by_project(project_id)? {
            if self.ticket_for_experiment(&experiment.id)?.is_some() {
                continue;
            }
            let title = experiment
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| experiment.slug.clone());
            let status = self.imported_status(&experiment)?;
            shown.insert(
                experiment.id.clone(),
                json!({"title": title, "status": status.as_str(), "archived": experiment.archived}),
            );
            nodes.push(import::Node {
                id: experiment.id.clone(),
                title,
                created_at: experiment.created_at,
            });
        }
        if nodes.is_empty() {
            bail!("Every experiment in this project is already on the board; there is nothing to import.");
        }
        let ids: Vec<String> = nodes.iter().map(|node| node.id.clone()).collect();
        let groups = match groups {
            None => import::draft(&nodes),
            Some(groups) => {
                let mut resolved = Vec::new();
                let mut placed = Vec::new();
                for group in groups {
                    let mut members = Vec::new();
                    for wanted in &group.experiment_ids {
                        let matches: Vec<&String> = ids
                            .iter()
                            .filter(|id| id.starts_with(wanted.as_str()))
                            .collect();
                        match matches.as_slice() {
                            [id] if !placed.contains(*id) => {
                                placed.push((*id).clone());
                                members.push((*id).clone());
                            }
                            [_] => bail!("Experiment {wanted} is in more than one group."),
                            [] => bail!(
                                "No experiment `{wanted}` waits to be imported in this project."
                            ),
                            _ => bail!(
                                "`{wanted}` matches more than one experiment; give more of its id."
                            ),
                        }
                    }
                    resolved.push(import::Group {
                        title: group.title,
                        experiment_ids: members,
                    });
                }
                let rest: Vec<String> = ids
                    .iter()
                    .filter(|id| !placed.contains(id))
                    .cloned()
                    .collect();
                if !rest.is_empty() {
                    resolved.push(import::Group {
                        title: import::UNSORTED.into(),
                        experiment_ids: rest,
                    });
                }
                import::validate(&resolved, &ids).map_err(|why| anyhow!("{why}"))?
            }
        };
        let payload = json!({"groups": groups, "nodes": shown, "by": actor.stored()});
        let tx = self.conn.unchecked_transaction()?;
        let open: Option<String> = tx
            .query_row(
                "SELECT id FROM board_decisions WHERE project_id = ?1 AND kind = 'import' AND status = 'open'",
                [project_id],
                |row| row.get(0),
            )
            .optional()?;
        let id = match open {
            Some(id) => {
                tx.execute(
                    "UPDATE board_decisions SET payload_json = ?2 WHERE id = ?1",
                    params![id, payload.to_string()],
                )?;
                id
            }
            None => Self::insert_decision(
                &tx,
                project_id,
                None,
                DecisionKind::Import,
                &[],
                &payload,
                &format!("import:{project_id}:{}", uuid::Uuid::new_v4()),
            )?,
        };
        tx.commit()?;
        self.require_decision(&id)
    }

    /// An imported node is done when one of its runs finished; otherwise it was
    /// never answered and comes in cancelled.
    fn imported_status(
        &self,
        experiment: &crate::local::model::LocalExperiment,
    ) -> Result<TicketStatus> {
        if experiment.archived {
            return Ok(TicketStatus::Cancelled);
        }
        let finished = self
            .list_runs_by_experiment(&experiment.id)?
            .iter()
            .any(|run| run.status == "done");
        Ok(if finished {
            TicketStatus::Done
        } else {
            TicketStatus::Cancelled
        })
    }

    /// Writes the confirmed grouping: one answered epic per group, one closed
    /// ticket per node, all at once.
    fn apply_import(&self, decision: &Decision, choice: &DecisionChoice) -> Result<()> {
        let nodes = decision.payload["nodes"]
            .as_object()
            .ok_or_else(|| anyhow!("This import card lists no experiments."))?;
        let ids: Vec<String> = nodes.keys().cloned().collect();
        let groups = match &choice.groups {
            Some(groups) => groups.clone(),
            None => serde_json::from_value(decision.payload["groups"].clone())?,
        };
        let groups = import::validate(&groups, &ids).map_err(|why| anyhow!("{why}"))?;
        let board = self.require_board_project(&decision.project_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let now = now_ms();
        let mut epics = 0;
        let mut tickets = 0;
        for group in &groups {
            let seq: i64 = tx.query_row(
                "UPDATE board_projects SET next_epic = next_epic + 1 WHERE project_id = ?1 RETURNING next_epic - 1",
                [&decision.project_id],
                |row| row.get(0),
            )?;
            let epic_id = uuid::Uuid::new_v4().to_string();
            let epic_key = format!("{}-E{seq}", board.key_prefix);
            tx.execute(
                "INSERT INTO board_epics (id, project_id, key, title, question, status, priority,
                     rules_json, proposed_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4, 'answered', 100, ?5, 'user', ?6, ?6)",
                params![
                    epic_id,
                    decision.project_id,
                    epic_key,
                    group.title,
                    serde_json::to_string(&EpicRules::default())?,
                    now
                ],
            )?;
            Self::append_board_event(
                &tx,
                &decision.project_id,
                &epic_id,
                &Actor::User,
                "imported",
                &json!({"key": epic_key, "decision": decision.id}),
            )?;
            epics += 1;
            for experiment_id in &group.experiment_ids {
                let taken: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM board_tickets WHERE experiment_id = ?1)",
                    [experiment_id],
                    |row| row.get(0),
                )?;
                if taken {
                    bail!("Experiment {experiment_id} got a ticket after this card was drafted; run the import again.");
                }
                let experiment = self.get_local_experiment(experiment_id)?.ok_or_else(|| {
                    anyhow!("Experiment {experiment_id} no longer exists; run the import again.")
                })?;
                let status = self.imported_status(&experiment)?;
                let seq: i64 = tx.query_row(
                    "UPDATE board_projects SET next_ticket = next_ticket + 1 WHERE project_id = ?1 RETURNING next_ticket - 1",
                    [&decision.project_id],
                    |row| row.get(0),
                )?;
                let ticket_id = uuid::Uuid::new_v4().to_string();
                let key = format!("{}-{seq:03}", board.key_prefix);
                let title = nodes[experiment_id]["title"]
                    .as_str()
                    .unwrap_or(&experiment.slug)
                    .to_string();
                tx.execute(
                    "INSERT INTO board_tickets (id, project_id, epic_id, key, kind, title, status,
                         base_experiment_id, experiment_id, conclusion, proposed_by, finished_at,
                         created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'experiment', ?5, ?6, ?7, ?8, ?9, 'user', ?10, ?11, ?10)",
                    params![
                        ticket_id,
                        decision.project_id,
                        epic_id,
                        key,
                        title,
                        status.as_str(),
                        experiment.parent_experiment_id,
                        experiment.id,
                        experiment.description.filter(|text| !text.trim().is_empty()),
                        now,
                        experiment.created_at
                    ],
                )?;
                Self::append_board_event(
                    &tx,
                    &decision.project_id,
                    &ticket_id,
                    &Actor::User,
                    "imported",
                    &json!({"key": key, "experimentId": experiment.id, "decision": decision.id}),
                )?;
                tickets += 1;
            }
        }
        Self::append_board_event(
            &tx,
            &decision.project_id,
            &decision.id,
            &Actor::User,
            "import_applied",
            &json!({"epics": epics, "tickets": tickets}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Lets a confidential project be published only on a one-time permit the
    /// user granted on a publish card; otherwise opens that card and refuses.
    pub fn gate_publish(&self, project_id: &str, what: &str) -> Result<()> {
        if !self
            .board_project(project_id)?
            .is_some_and(|board| board.enabled && board.confidential)
        {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        let permit: Option<String> = tx
            .query_row(
                "SELECT d.id FROM board_decisions d
                 WHERE d.project_id = ?1 AND d.kind = 'publish_gate' AND d.status = 'resolved'
                   AND json_extract(d.resolution_json, '$.option') = 'allow'
                   AND NOT EXISTS(SELECT 1 FROM board_events e
                                  WHERE e.subject_id = d.id AND e.kind = 'publish_permit_used')
                 ORDER BY d.resolved_at LIMIT 1",
                [project_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(decision_id) = permit {
            Self::append_board_event(
                &tx,
                project_id,
                &decision_id,
                &Actor::System(None),
                "publish_permit_used",
                &json!({"what": what}),
            )?;
            tx.commit()?;
            return Ok(());
        }
        let open = tx
            .query_row(
                "SELECT 1 FROM board_decisions
                 WHERE project_id = ?1 AND kind = 'publish_gate' AND status = 'open'",
                [project_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !open {
            Self::insert_decision(
                &tx,
                project_id,
                None,
                DecisionKind::PublishGate,
                &[],
                &json!({"what": what}),
                &format!("publish:{project_id}:{}", uuid::Uuid::new_v4()),
            )?;
        }
        tx.commit()?;
        bail!(
            "This project is confidential until its patent is filed, so publishing it ({what}) needs the user's \
             permission. A publish card is waiting on the research board; once the user allows it, run this again."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::model::LocalProject;

    fn store() -> Store {
        let dir = std::env::temp_dir().join(format!("orx-board-{}", uuid::Uuid::new_v4()));
        Store::open_at(dir).unwrap()
    }

    fn project(store: &Store, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create_local_project(&LocalProject {
                id: id.clone(),
                name: name.into(),
                slug: id.clone(),
                github_owner: String::new(),
                github_repo: String::new(),
                github_sync_enabled: false,
                baseline_branch: "main".into(),
                repo_path: std::env::temp_dir()
                    .join(&id)
                    .to_string_lossy()
                    .into_owned(),
                run_command: None,
                paper_id: None,
                icon: None,
                created_at: 1,
                updated_at: 1,
            })
            .unwrap();
        id
    }

    fn agent() -> Actor {
        Actor::Agent("chat-1".into())
    }

    fn experiment(
        store: &Store,
        project_id: &str,
        epic: &Epic,
        title: &str,
        batch: Option<&str>,
    ) -> Ticket {
        store
            .create_ticket(
                &NewTicket {
                    project_id: project_id.into(),
                    epic_id: Some(epic.id.clone()),
                    kind: TicketKind::Experiment,
                    batch_id: batch.map(str::to_string),
                    title: title.into(),
                    hypothesis: Some("lower lr converges faster".into()),
                    change_spec: Some("config.yaml lr".into()),
                    priority: 100,
                    base_experiment_id: None,
                    needs_gpus: 1,
                    est_minutes: Some(90),
                    validation: Some(json!({"metric": "val_loss", "every": "500 steps"})),
                    harness: None,
                    core_hypothesis: false,
                },
                &agent(),
            )
            .unwrap()
    }

    fn active_epic(store: &Store, project_id: &str) -> Epic {
        let epic = store
            .create_epic(
                &NewEpic {
                    project_id: project_id.into(),
                    title: "Learning rate".into(),
                    question: "Which LR converges fastest?".into(),
                    priority: 100,
                    budget_gpu_hours: Some(24.0),
                    budget_deadline_ms: None,
                },
                &agent(),
            )
            .unwrap();
        let criteria = store
            .propose_criteria(
                &epic.id,
                "Lowest val loss at 10k steps over 3 seeds.",
                &json!({"minSeeds": 3}),
                &agent(),
            )
            .unwrap();
        store
            .approve_criteria(&epic.id, criteria.version, &Actor::User)
            .unwrap()
    }

    #[test]
    fn tables_survive_reopening_the_store() {
        let dir = std::env::temp_dir().join(format!("orx-board-{}", uuid::Uuid::new_v4()));
        let id = {
            let store = Store::open_at(dir.clone()).unwrap();
            let id = project(&store, "Slot JEPA");
            store
                .update_board_project(
                    &id,
                    &BoardSettingsUpdate {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )
                .unwrap();
            id
        };
        let store = Store::open_at(dir).unwrap();
        let board = store.board_project(&id).unwrap().unwrap();
        assert!(board.enabled && board.confidential);
        assert_eq!(board.key_prefix, "SJ");
    }

    #[test]
    fn prefixes_come_from_the_name_and_stay_unique() {
        assert_eq!(default_key_prefix("Slot-JEPA"), "SJ");
        assert_eq!(default_key_prefix("INTACT-JEPA"), "IJ");
        assert_eq!(default_key_prefix("facecam"), "FAC");
        assert_eq!(default_key_prefix("—"), "T");
        let store = store();
        let first = project(&store, "Slot JEPA");
        let second = project(&store, "Super Jumps");
        store
            .update_board_project(&first, &BoardSettingsUpdate::default())
            .unwrap();
        let error = store
            .update_board_project(&second, &BoardSettingsUpdate::default())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("`SJ`") && error.contains("--prefix"),
            "{error}"
        );
        let board = store
            .update_board_project(
                &second,
                &BoardSettingsUpdate {
                    key_prefix: Some("sx".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(board.key_prefix, "SX");
    }

    #[test]
    fn a_ticket_missing_its_estimate_is_amended_then_approved() {
        let store = store();
        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(&id, &BoardSettingsUpdate::default())
            .unwrap();
        let epic = active_epic(&store, &id);
        let ticket = store
            .create_ticket(
                &NewTicket {
                    project_id: id.clone(),
                    epic_id: Some(epic.id.clone()),
                    kind: TicketKind::Experiment,
                    batch_id: None,
                    title: "mask moments".into(),
                    hypothesis: None,
                    change_spec: None,
                    priority: 100,
                    base_experiment_id: None,
                    needs_gpus: 1,
                    est_minutes: None,
                    validation: None,
                    harness: None,
                    core_hypothesis: false,
                },
                &agent(),
            )
            .unwrap();
        let refused = store
            .move_ticket(&ticket.id, TicketAction::Approve, &Actor::User, Value::Null)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("orx ticket amend SJ-001"), "{refused}");
        let wrong = store.amend_ticket(&ticket.id, None, Some(json!([1])), None, &agent());
        assert!(wrong.is_err(), "validation must be an object");
        let amended = store
            .amend_ticket(
                &ticket.id,
                Some(30),
                Some(json!({"metrics": ["pose_px"]})),
                None,
                &agent(),
            )
            .unwrap();
        assert_eq!(
            (amended.est_minutes, amended.needs_gpus, amended.status),
            (Some(30), 1, TicketStatus::Proposed),
            "the agent fills it in; it still waits for the user"
        );
        let approved = store
            .move_ticket(&ticket.id, TicketAction::Approve, &Actor::User, Value::Null)
            .unwrap();
        assert_eq!(approved.status, TicketStatus::Ready);
        let late = store
            .amend_ticket(&ticket.id, Some(60), None, None, &agent())
            .unwrap_err()
            .to_string();
        assert!(late.contains("only a proposed ticket"), "{late}");
    }

    #[test]
    fn a_batch_proposed_by_an_agent_lands_on_one_approval_card() {
        let store = store();
        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(&id, &BoardSettingsUpdate::default())
            .unwrap();
        let epic = active_epic(&store, &id);
        let tickets: Vec<Ticket> = ["lr 1e-4", "lr 3e-4", "lr 1e-3"]
            .iter()
            .map(|title| experiment(&store, &id, &epic, title, Some("round-1")))
            .collect();
        assert_eq!(tickets[0].key, "SJ-001");
        assert_eq!(tickets[2].key, "SJ-003");
        assert!(tickets
            .iter()
            .all(|ticket| ticket.status == TicketStatus::Proposed));
        let cards: Vec<Decision> = store
            .open_decisions(Some(&id))
            .unwrap()
            .into_iter()
            .filter(|decision| decision.kind == DecisionKind::ApproveBatch)
            .collect();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].ticket_ids.len(), 3);

        // Unticking one and confirming approves exactly the other two.
        let decision = store
            .resolve_decision(
                &cards[0].id,
                &DecisionChoice {
                    selected: Some(vec!["SJ-001".into(), "SJ-003".into()]),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        assert_eq!(decision.status, DecisionStatus::Resolved);
        let statuses: Vec<TicketStatus> = ["SJ-001", "SJ-002", "SJ-003"]
            .iter()
            .map(|key| store.require_ticket(key).unwrap().status)
            .collect();
        assert_eq!(
            statuses,
            [
                TicketStatus::Ready,
                TicketStatus::Rejected,
                TicketStatus::Ready
            ]
        );
    }

    #[test]
    fn agents_cannot_approve_and_unready_tickets_name_what_they_lack() {
        let store = store();
        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(&id, &BoardSettingsUpdate::default())
            .unwrap();
        let epic = store
            .create_epic(
                &NewEpic {
                    project_id: id.clone(),
                    title: "Arch".into(),
                    question: "Wider?".into(),
                    priority: 100,
                    budget_gpu_hours: None,
                    budget_deadline_ms: None,
                },
                &agent(),
            )
            .unwrap();
        let ticket = experiment(&store, &id, &epic, "wider mlp", None);
        let card = store
            .open_decisions(Some(&id))
            .unwrap()
            .into_iter()
            .find(|d| d.kind == DecisionKind::ApproveBatch)
            .unwrap();
        assert!(store
            .resolve_decision(&card.id, &DecisionChoice::default(), &agent())
            .is_err());
        assert!(store
            .move_ticket(&ticket.key, TicketAction::Approve, &agent(), Value::Null)
            .is_err());
        let error = store
            .move_ticket(
                &ticket.key,
                TicketAction::Approve,
                &Actor::User,
                Value::Null,
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("answer criteria") && error.contains("SJ-E1"),
            "{error}"
        );
        assert_eq!(
            store.require_ticket(&ticket.key).unwrap().status,
            TicketStatus::Proposed
        );
    }

    #[test]
    fn a_question_pauses_only_its_ticket_until_the_user_answers() {
        let store = store();
        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(&id, &BoardSettingsUpdate::default())
            .unwrap();
        let epic = active_epic(&store, &id);
        let asked = experiment(&store, &id, &epic, "lr 1e-4", Some("r1"));
        let other = experiment(&store, &id, &epic, "lr 3e-4", Some("r1"));
        let card = store
            .open_decisions(Some(&id))
            .unwrap()
            .into_iter()
            .find(|d| d.kind == DecisionKind::ApproveBatch)
            .unwrap();
        store
            .resolve_decision(&card.id, &DecisionChoice::default(), &Actor::User)
            .unwrap();
        let board = Actor::System(None);
        for key in [&asked.key, &other.key] {
            store
                .move_ticket(key, TicketAction::Dispatch, &board, Value::Null)
                .unwrap();
        }
        assert!(store
            .ask_on_ticket(&asked.key, "Eval split?", &["val".into()], None, &agent())
            .is_err());
        let question = store
            .ask_on_ticket(
                &asked.key,
                "Eval split?",
                &["val".into(), "test".into()],
                Some("val"),
                &agent(),
            )
            .unwrap();
        assert_eq!(
            store.require_ticket(&asked.key).unwrap().status,
            TicketStatus::Waiting
        );
        assert_eq!(
            store.require_ticket(&other.key).unwrap().status,
            TicketStatus::Running
        );
        assert!(store
            .resolve_decision(
                &question.id,
                &DecisionChoice {
                    option: Some("train".into()),
                    ..Default::default()
                },
                &Actor::User
            )
            .is_err());
        store
            .resolve_decision(
                &question.id,
                &DecisionChoice {
                    option: Some("val".into()),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        assert_eq!(
            store.require_ticket(&asked.key).unwrap().status,
            TicketStatus::Running
        );
        let kinds: Vec<String> = store
            .board_events(&asked.id)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            ["ticket_proposed", "approve", "dispatch", "ask", "answer"]
        );
    }

    #[test]
    fn a_report_goes_to_review_and_only_done_results_can_be_tainted() {
        let store = store();
        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(&id, &BoardSettingsUpdate::default())
            .unwrap();
        let epic = active_epic(&store, &id);
        let ticket = experiment(&store, &id, &epic, "lr 1e-4", None);
        store
            .move_ticket(
                &ticket.key,
                TicketAction::Approve,
                &Actor::User,
                Value::Null,
            )
            .unwrap();
        store
            .move_ticket(
                &ticket.key,
                TicketAction::Dispatch,
                &Actor::System(None),
                Value::Null,
            )
            .unwrap();
        assert!(store
            .taint_ticket(&ticket.key, "eval bug", &Actor::User)
            .is_err());
        let reported = store
            .report_ticket(
                &ticket.key,
                Outcome::Refutes,
                "No faster than baseline.",
                Some(&json!({"seeds": 3})),
                &agent(),
            )
            .unwrap();
        assert_eq!(reported.status, TicketStatus::Review);
        assert_eq!(reported.outcome, Some(Outcome::Refutes));
        assert!(reported.finished_at.is_some());
        store
            .move_ticket(&ticket.key, TicketAction::Accept, &Actor::User, Value::Null)
            .unwrap();
        assert!(store
            .taint_ticket(&ticket.key, "eval bug", &agent())
            .is_err());
        let tainted = store
            .taint_ticket(&ticket.key, "eval used the train split", &Actor::User)
            .unwrap();
        assert!(tainted.tainted && tainted.status == TicketStatus::Done);
    }

    #[test]
    fn a_newer_criteria_draft_supersedes_the_open_one() {
        let store = store();
        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(&id, &BoardSettingsUpdate::default())
            .unwrap();
        let epic = store
            .create_epic(
                &NewEpic {
                    project_id: id.clone(),
                    title: "Arch".into(),
                    question: "Wider?".into(),
                    priority: 100,
                    budget_gpu_hours: None,
                    budget_deadline_ms: None,
                },
                &agent(),
            )
            .unwrap();
        store
            .propose_criteria(&epic.id, "v1", &json!({}), &agent())
            .unwrap();
        store
            .propose_criteria(&epic.id, "v2", &json!({}), &agent())
            .unwrap();
        let open: Vec<Decision> = store.open_decisions(Some(&id)).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].payload["version"], 2);
        store
            .resolve_decision(&open[0].id, &DecisionChoice::default(), &Actor::User)
            .unwrap();
        let epic = store.require_epic(&epic.key).unwrap();
        assert_eq!(
            (epic.status, epic.criteria_version),
            (EpicStatus::Active, 2)
        );
    }

    #[test]
    fn machines_take_known_backends_and_deleting_a_project_clears_its_board() {
        let store = store();
        assert!(store.upsert_machine("box", "modal", 1, true).is_err());
        store.upsert_machine("ws", "local", 2, true).unwrap();
        let machine = store.upsert_machine("ws", "ssh:gpu1", 4, true).unwrap();
        assert_eq!(
            (machine.backend.as_str(), machine.gpu_slots),
            ("ssh:gpu1", 4)
        );
        assert_eq!(store.list_machines().unwrap().len(), 1);

        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(&id, &BoardSettingsUpdate::default())
            .unwrap();
        let epic = active_epic(&store, &id);
        experiment(&store, &id, &epic, "lr", None);
        store.delete_local_project(&id).unwrap();
        assert!(store.board_project(&id).unwrap().is_none());
        assert!(store.list_tickets(&id).unwrap().is_empty());
        assert!(store.open_decisions(Some(&id)).unwrap().is_empty());
    }

    fn board_with_epic(name: &str) -> (Store, String, Epic) {
        let store = store();
        let id = project(&store, name);
        store
            .update_board_project(
                &id,
                &BoardSettingsUpdate {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let epic = active_epic(&store, &id);
        (store, id, epic)
    }

    fn approved(store: &Store, project_id: &str, epic: &Epic, title: &str) -> Ticket {
        let ticket = experiment(store, project_id, epic, title, None);
        store
            .move_ticket(
                &ticket.key,
                TicketAction::Approve,
                &Actor::User,
                Value::Null,
            )
            .unwrap()
    }

    fn running(store: &Store, project_id: &str, epic: &Epic, title: &str) -> Ticket {
        let ticket = approved(store, project_id, epic, title);
        store.dispatch_ticket(&ticket.id, None).unwrap()
    }

    fn cards(store: &Store, project_id: &str, kind: DecisionKind) -> Vec<Decision> {
        store
            .open_decisions(Some(project_id))
            .unwrap()
            .into_iter()
            .filter(|decision| decision.kind == kind)
            .collect()
    }

    #[test]
    fn the_scheduler_sees_only_ready_tickets_on_enabled_boards_and_counts_held_slots() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        let machine = store.upsert_machine("ws", "local", 2, true).unwrap();
        let first = approved(&store, &id, &epic, "lr 1e-4");
        let _second = approved(&store, &id, &epic, "lr 3e-4");
        experiment(&store, &id, &epic, "still proposed", None);
        assert!(store.board_has_live_work().unwrap());
        let candidates = store.dispatch_candidates().unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates
            .iter()
            .all(|c| c.confidential && c.needs_gpus == 1 && !c.out_of_budget));

        store.dispatch_ticket(&first.id, Some(&machine.id)).unwrap();
        let slots = store.machine_slots().unwrap();
        assert_eq!((slots[0].gpu_slots, slots[0].in_use), (2, 1));
        // The same ticket cannot be dispatched twice.
        assert!(store.dispatch_ticket(&first.id, Some(&machine.id)).is_err());

        store
            .update_board_project(
                &id,
                &BoardSettingsUpdate {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(store.dispatch_candidates().unwrap().is_empty());
    }

    #[test]
    fn a_dependency_holds_a_ticket_until_the_other_is_done() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        let first = approved(&store, &id, &epic, "probe");
        let second = approved(&store, &id, &epic, "follow-up");
        store
            .conn
            .execute(
                "INSERT INTO board_relations (from_id, to_id, kind, created_at) VALUES (?1, ?2, 'blocked_by', 0)",
                params![second.id, first.id],
            )
            .unwrap();
        let waiting: Vec<bool> = store
            .dispatch_candidates()
            .unwrap()
            .iter()
            .map(|c| c.waiting_on_dependency)
            .collect();
        assert_eq!(waiting, [false, true]);
    }

    #[test]
    fn a_ticket_that_keeps_failing_to_start_is_blocked_with_a_card() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        let ticket = approved(&store, &id, &epic, "lr 1e-4");
        for attempt in 1..=3 {
            store.dispatch_ticket(&ticket.id, None).unwrap();
            let after = store
                .requeue_ticket(&ticket.id, "agent not signed in")
                .unwrap();
            let expected = if attempt < 3 {
                TicketStatus::Ready
            } else {
                TicketStatus::Blocked
            };
            assert_eq!(after.status, expected, "attempt {attempt}");
            assert!(after.session_id.is_none() && after.machine_id.is_none());
        }
        let blocked = cards(&store, &id, DecisionKind::Blocked);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].payload["reason"], "agent not signed in");
        store
            .resolve_decision(
                &blocked[0].id,
                &DecisionChoice {
                    option: Some("unblock".into()),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        assert_eq!(
            store.require_ticket(&ticket.key).unwrap().status,
            TicketStatus::Ready
        );
        assert!(cards(&store, &id, DecisionKind::Blocked).is_empty());
    }

    #[test]
    fn checked_results_share_a_batch_card_and_failing_or_core_refutations_stand_alone() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        store
            .set_board_setting(
                "checks",
                &json!({"metric": "val_loss", "higherIsBetter": false, "noiseFloor": 0.01}),
                &Actor::User,
            )
            .unwrap();
        let good = running(&store, &id, &epic, "lr 3e-4");
        let negative = running(&store, &id, &epic, "lr 1e-2");
        let sloppy = running(&store, &id, &epic, "lr 1e-3");
        let core = experiment(&store, &id, &epic, "no role head", None);
        store
            .conn
            .execute(
                "UPDATE board_tickets SET core_hypothesis = 1 WHERE id = ?1",
                [&core.id],
            )
            .unwrap();
        store
            .move_ticket(&core.key, TicketAction::Approve, &Actor::User, Value::Null)
            .unwrap();
        store.dispatch_ticket(&core.id, None).unwrap();

        let report = |ticket: &Ticket, outcome, values: Value| {
            store
                .report_ticket(
                    &ticket.key,
                    outcome,
                    "see numbers",
                    Some(&json!({"metric": "val_loss", "values": values, "baseline": 2.0})),
                    &agent(),
                )
                .unwrap()
        };
        assert!(store
            .report_ticket(&good.key, Outcome::Supports, "x", None, &agent())
            .is_err());
        report(&good, Outcome::Supports, json!([1.8, 1.82, 1.79]));
        report(&negative, Outcome::Refutes, json!([2.3, 2.31, 2.29]));
        report(&sloppy, Outcome::Supports, json!([1.9, null]));
        report(&core, Outcome::Refutes, json!([2.3, 2.3, 2.3]));

        let batch = cards(&store, &id, DecisionKind::AcceptBatch);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].ticket_ids, [good.id.clone(), negative.id.clone()]);
        assert_eq!(batch[0].payload["verdicts"][&good.id]["passed"], true);
        let alone: Vec<Vec<String>> = cards(&store, &id, DecisionKind::AcceptOne)
            .into_iter()
            .map(|d| d.ticket_ids)
            .collect();
        assert_eq!(alone, [vec![sloppy.id.clone()], vec![core.id.clone()]]);

        // Unticking the negative result accepts the other and gives it a card of its own.
        store
            .resolve_decision(
                &batch[0].id,
                &DecisionChoice {
                    selected: Some(vec![good.key.clone()]),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        assert_eq!(
            store.require_ticket(&good.key).unwrap().status,
            TicketStatus::Done
        );
        assert_eq!(
            store.require_ticket(&negative.key).unwrap().status,
            TicketStatus::Review
        );
        let own = cards(&store, &id, DecisionKind::AcceptOne);
        assert_eq!(own.len(), 3);
        let negative_card = own
            .iter()
            .find(|d| d.ticket_ids == [negative.id.clone()])
            .unwrap();
        assert!(store
            .resolve_decision(
                &negative_card.id,
                &DecisionChoice {
                    option: Some("challenge".into()),
                    ..Default::default()
                },
                &Actor::User
            )
            .is_err());
        store
            .resolve_decision(
                &negative_card.id,
                &DecisionChoice {
                    option: Some("challenge".into()),
                    text: Some("rerun with seed 4".into()),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        assert_eq!(
            store.require_ticket(&negative.key).unwrap().status,
            TicketStatus::Running
        );
    }

    #[test]
    fn auto_done_accepts_passing_results_without_a_card() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        let rules = EpicRules {
            auto_done: true,
            ..EpicRules::default()
        };
        store
            .set_epic_rules(&epic.id, &rules, &Actor::User)
            .unwrap();
        let ticket = running(&store, &id, &epic, "lr 3e-4");
        let done = store
            .report_ticket(
                &ticket.key,
                Outcome::Supports,
                "faster",
                Some(&json!({"values": [0.7, 0.71, 0.69], "baseline": 0.6})),
                &agent(),
            )
            .unwrap();
        assert_eq!(done.status, TicketStatus::Done);
        assert!(cards(&store, &id, DecisionKind::AcceptBatch).is_empty());
        let accept = store
            .board_events(&ticket.id)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "accept")
            .unwrap();
        assert_eq!(accept.actor, "system:auto_done");
    }

    #[test]
    fn a_tickets_chat_links_its_first_experiment_and_each_run_end_is_logged_once() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        let ticket = running(&store, &id, &epic, "lr 3e-4");
        store.bind_ticket_session(&ticket.id, "chat-7").unwrap();
        assert_eq!(
            store.ticket_for_session("chat-7").unwrap().unwrap().id,
            ticket.id
        );
        store.link_ticket_experiment("chat-7", "exp-1").unwrap();
        store.link_ticket_experiment("chat-7", "exp-2").unwrap();
        store.link_ticket_experiment("chat-other", "exp-3").unwrap();
        let linked = store.ticket_for_experiment("exp-1").unwrap().unwrap();
        assert_eq!(linked.id, ticket.id);
        assert!(store.ticket_for_experiment("exp-2").unwrap().is_none());
        assert!(store
            .record_ticket_run_end(&linked, "run-1", "done")
            .unwrap());
        assert!(!store
            .record_ticket_run_end(&linked, "run-1", "done")
            .unwrap());
        assert!(store
            .record_ticket_run_end(&linked, "run-2", "failed")
            .unwrap());
    }

    #[test]
    fn the_users_answers_and_comments_wait_for_delivery_and_cancelled_chats_for_a_stop() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        let ticket = running(&store, &id, &epic, "lr 3e-4");
        store
            .comment_on_ticket(&ticket.key, "before it had a chat", &Actor::User)
            .unwrap();
        assert!(store.undelivered_ticket_messages().unwrap().is_empty());
        store.bind_ticket_session(&ticket.id, "chat-7").unwrap();
        store
            .comment_on_ticket(&ticket.key, "agent note", &agent())
            .unwrap();
        let question = store
            .ask_on_ticket(
                &ticket.key,
                "Which split?",
                &["val".into(), "test".into()],
                Some("val"),
                &agent(),
            )
            .unwrap();
        store
            .resolve_decision(
                &question.id,
                &DecisionChoice {
                    option: Some("val".into()),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        let pending = store.undelivered_ticket_messages().unwrap();
        let kinds: Vec<&str> = pending
            .iter()
            .map(|(event, _)| event.kind.as_str())
            .collect();
        assert_eq!(
            kinds,
            ["comment", "answer"],
            "the agent's own note is not sent back to it"
        );
        for (event, ticket) in &pending {
            store
                .mark_ticket_message_delivered(ticket, event.id, "chat-7")
                .unwrap();
        }
        assert!(store.undelivered_ticket_messages().unwrap().is_empty());

        assert!(store.tickets_to_stop().unwrap().is_empty());
        let cancelled = store
            .move_ticket(&ticket.key, TicketAction::Cancel, &Actor::User, Value::Null)
            .unwrap();
        assert_eq!(store.tickets_to_stop().unwrap().len(), 1);
        store
            .note_ticket_once(&cancelled, "chat_stopped", &json!({}))
            .unwrap();
        assert!(store.tickets_to_stop().unwrap().is_empty());
    }

    /// Records a turn in `session` that a message with these parts started.
    fn turn(store: &Store, session: &str, parts: Value) {
        let message = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        store
            .conn
            .execute(
                "INSERT INTO chat_messages (id, session_id, role, parts_json, created_at) VALUES (?1, ?2, 'user', ?3, ?4)",
                params![message, session, parts.to_string(), now],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO chat_turns (id, session_id, user_message_id, assistant_message_id, client_turn_id,
                     request_hash, prepared_input, settings_json, state, delivery_state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'a', ?1, '', '', '{}', 'running', 'pending', ?4, ?4)",
                params![uuid::Uuid::new_v4().to_string(), session, message, now],
            )
            .unwrap();
    }

    fn typed(store: &Store, session: &str) {
        turn(
            store,
            session,
            json!([{"id": "p0", "type": "text", "text": "try lr 1e-3"}]),
        );
    }

    fn notified(store: &Store, session: &str) {
        turn(
            store,
            session,
            json!([{"id": "p0", "type": "text", "text": "run ended", "orx": {"kind": "message"}}]),
        );
    }

    #[test]
    fn a_user_asking_in_chat_opens_an_unplanned_ticket_and_an_agent_on_its_own_is_refused() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        typed(&store, "chat-u");
        store
            .gate_experiment_create(&id, Some("chat-u"), Some("lr 1e-3"))
            .unwrap();
        let unplanned = store.ticket_for_session("chat-u").unwrap().unwrap();
        assert_eq!(
            (
                unplanned.status,
                unplanned.proposed_by,
                unplanned.title.as_str()
            ),
            (TicketStatus::Running, ProposedBy::UserChat, "lr 1e-3")
        );
        assert_eq!(store.board_events(&unplanned.id).unwrap()[0].actor, "user");
        // A second create in the same turn belongs to the same ticket.
        store
            .gate_experiment_create(&id, Some("chat-u"), None)
            .unwrap();
        store.link_ticket_experiment("chat-u", "exp-1").unwrap();
        assert_eq!(store.list_tickets(&id).unwrap().len(), 1);

        // The agent's own follow-up turn may not branch out on its own.
        notified(&store, "chat-u");
        let refused = store
            .gate_experiment_create(&id, Some("chat-u"), None)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("orx ticket propose"), "{refused}");
        // The user's next request is a ticket of its own.
        typed(&store, "chat-u");
        store
            .gate_experiment_create(&id, Some("chat-u"), Some("lr 3e-3"))
            .unwrap();
        assert_eq!(store.list_tickets(&id).unwrap().len(), 2);

        // An executor creates for its running ticket even though orx wrote its turn.
        let ticket = running(&store, &id, &epic, "lr 3e-4");
        store.bind_ticket_session(&ticket.id, "chat-x").unwrap();
        notified(&store, "chat-x");
        store
            .gate_experiment_create(&id, Some("chat-x"), None)
            .unwrap();
        // A chat with no turn of the user's is an agent acting alone.
        assert!(store
            .gate_experiment_create(&id, Some("chat-none"), None)
            .is_err());
        // The user's own terminal and projects without a board are not gated.
        store.gate_experiment_create(&id, None, None).unwrap();
        let plain = project(&store, "plain");
        store
            .gate_experiment_create(&plain, Some("chat-none"), None)
            .unwrap();
    }

    fn local_experiment(store: &Store, project_id: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        store
            .create_local_experiment(&crate::local::model::LocalExperiment {
                id: id.clone(),
                project_id: project_id.into(),
                parent_experiment_id: None,
                slug: id.clone(),
                branch_name: format!("orx/{id}"),
                title: None,
                description: None,
                run_command: String::new(),
                agent_status: "idle".into(),
                created_at: 1,
                updated_at: 1,
                chat_session_id: None,
                archived: false,
            })
            .unwrap();
        id
    }

    #[test]
    fn runs_launch_on_the_tickets_machine_and_confidential_runs_stay_on_owned_ones() {
        let (store, id, epic) = board_with_epic("Slot JEPA");
        let lab = store.upsert_machine("lab", "ssh:lab", 2, true).unwrap();
        store
            .upsert_machine("rented", "ssh:rented", 8, false)
            .unwrap();
        let ticket = approved(&store, &id, &epic, "lr 3e-4");
        store.dispatch_ticket(&ticket.id, Some(&lab.id)).unwrap();
        store.bind_ticket_session(&ticket.id, "chat-x").unwrap();
        let experiment = local_experiment(&store, &id);
        store.link_ticket_experiment("chat-x", &experiment).unwrap();
        store
            .gate_launch(&experiment, Some("chat-x"), "ssh", Some("lab"))
            .unwrap();
        let elsewhere = store
            .gate_launch(&experiment, Some("chat-x"), "local", None)
            .unwrap_err()
            .to_string();
        assert!(
            elsewhere.contains("--backend ssh --host lab"),
            "{elsewhere}"
        );

        let loose = local_experiment(&store, &id);
        store.gate_launch(&loose, None, "local", None).unwrap();
        for (backend, host) in [("ssh", Some("rented")), ("modal", None)] {
            let refused = store
                .gate_launch(&loose, None, backend, host)
                .unwrap_err()
                .to_string();
            assert!(refused.contains("confidential"), "{refused}");
        }
        store
            .update_board_project(
                &id,
                &BoardSettingsUpdate {
                    confidential: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        store.gate_launch(&loose, None, "modal", None).unwrap();
    }

    #[test]
    fn publishing_a_confidential_project_takes_one_permit_per_publish() {
        let (store, id, _epic) = board_with_epic("Slot JEPA");
        let first = store
            .gate_publish(&id, "share --publish")
            .unwrap_err()
            .to_string();
        assert!(first.contains("publish card"), "{first}");
        assert!(store.gate_publish(&id, "share --publish").is_err());
        let card = cards(&store, &id, DecisionKind::PublishGate);
        assert_eq!(card.len(), 1, "a repeated attempt reuses the open card");
        assert!(store
            .resolve_decision(&card[0].id, &DecisionChoice::default(), &Actor::User)
            .is_err());
        let allow = DecisionChoice {
            option: Some("allow".into()),
            ..Default::default()
        };
        assert!(store
            .resolve_decision(&card[0].id, &allow, &agent())
            .is_err());
        store
            .resolve_decision(&card[0].id, &allow, &Actor::User)
            .unwrap();
        store.gate_publish(&id, "share --publish").unwrap();
        assert!(
            store.gate_publish(&id, "share --publish").is_err(),
            "the permit is used up"
        );
        let deny = DecisionChoice {
            option: Some("deny".into()),
            ..Default::default()
        };
        let second = cards(&store, &id, DecisionKind::PublishGate);
        store
            .resolve_decision(&second[0].id, &deny, &Actor::User)
            .unwrap();
        assert!(store.gate_publish(&id, "share --publish").is_err());
        let open = project(&store, "open");
        store.gate_publish(&open, "share --publish").unwrap();
    }

    fn titled_experiment(store: &Store, project_id: &str, title: &str, finished: bool) -> String {
        let id = local_experiment(store, project_id);
        store
            .conn
            .execute(
                "UPDATE local_experiments SET title = ?2, description = ?3,
                     created_at = (SELECT COUNT(*) FROM local_experiments) WHERE id = ?1",
                params![id, title, format!("notes on {title}")],
            )
            .unwrap();
        if finished {
            store
                .conn
                .execute(
                    "INSERT INTO runs (id, experiment_id, project_id, status, backend_json, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'done', '{}', 1, 1)",
                    params![uuid::Uuid::new_v4().to_string(), id, project_id],
                )
                .unwrap();
        }
        id
    }

    #[test]
    fn an_import_writes_nothing_until_the_user_confirms_the_grouping() {
        let store = store();
        let id = project(&store, "Slot JEPA");
        store
            .update_board_project(
                &id,
                &BoardSettingsUpdate {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let e1a = titled_experiment(&store, &id, "E1a robot role head: centroid", true);
        let e1b = titled_experiment(&store, &id, "E1b robot role head: contrastive", true);
        let e2 = titled_experiment(&store, &id, "E2 object role: contact head", false);
        let loose = titled_experiment(&store, &id, "Slot swap diagnostic", true);
        assert_eq!(store.unticketed_experiments(&id).unwrap(), 4);

        let card = store.propose_import(&id, None, &agent()).unwrap();
        let groups: Vec<import::Group> =
            serde_json::from_value(card.payload["groups"].clone()).unwrap();
        assert_eq!(
            groups
                .iter()
                .map(|g| (g.title.as_str(), g.experiment_ids.len()))
                .collect::<Vec<_>>(),
            [
                ("E1: robot role head", 2),
                ("E2: object role", 1),
                (import::UNSORTED, 1)
            ]
        );
        assert!(
            store.list_epics(&id).unwrap().is_empty(),
            "a draft writes no epics"
        );
        // Drafting again replaces the open card instead of stacking a second one.
        store.propose_import(&id, None, &Actor::User).unwrap();
        assert_eq!(cards(&store, &id, DecisionKind::Import).len(), 1);
        assert!(store
            .resolve_decision(
                &card.id,
                &DecisionChoice {
                    option: Some("import".into()),
                    ..Default::default()
                },
                &agent()
            )
            .is_err());

        // The user renames E1 and merges the diagnostic into it.
        let settled = vec![
            import::Group {
                title: "Robot role".into(),
                experiment_ids: vec![e1a.clone(), e1b.clone(), loose.clone()],
            },
            import::Group {
                title: "Object role".into(),
                experiment_ids: vec![e2.clone()],
            },
        ];
        store
            .resolve_decision(
                &card.id,
                &DecisionChoice {
                    option: Some("import".into()),
                    groups: Some(settled),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        let epics = store.list_epics(&id).unwrap();
        assert_eq!(
            epics
                .iter()
                .map(|epic| (epic.title.as_str(), epic.status))
                .collect::<Vec<_>>(),
            [
                ("Robot role", EpicStatus::Answered),
                ("Object role", EpicStatus::Answered)
            ]
        );
        let ticket = store.ticket_for_experiment(&e1a).unwrap().unwrap();
        assert_eq!(
            (ticket.status, ticket.proposed_by),
            (TicketStatus::Done, ProposedBy::User)
        );
        assert_eq!(
            ticket.conclusion.as_deref(),
            Some("notes on E1a robot role head: centroid")
        );
        assert_eq!(ticket.epic_id.as_deref(), Some(epics[0].id.as_str()));
        assert_eq!(
            store.ticket_for_experiment(&e2).unwrap().unwrap().status,
            TicketStatus::Cancelled
        );
        assert_eq!(store.unticketed_experiments(&id).unwrap(), 0);
        assert!(store
            .propose_import(&id, None, &Actor::User)
            .unwrap_err()
            .to_string()
            .contains("nothing to import"));
    }

    #[test]
    fn a_grouping_file_names_experiments_by_id_prefix_and_leaves_the_rest_unsorted() {
        let store = store();
        let id = project(&store, "INTACT");
        store
            .update_board_project(
                &id,
                &BoardSettingsUpdate {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let a = titled_experiment(&store, &id, "Joint-SAGE P1", true);
        let b = titled_experiment(&store, &id, "Probe v2", true);
        let wanted = vec![import::Group {
            title: "Joint-SAGE".into(),
            experiment_ids: vec![a[..8].to_string()],
        }];
        let card = store.propose_import(&id, Some(wanted), &agent()).unwrap();
        let groups: Vec<import::Group> =
            serde_json::from_value(card.payload["groups"].clone()).unwrap();
        assert_eq!(groups[0].experiment_ids, std::slice::from_ref(&a));
        assert_eq!(
            (groups[1].title.as_str(), groups[1].experiment_ids.clone()),
            (import::UNSORTED, vec![b])
        );
        let unknown = vec![import::Group {
            title: "x".into(),
            experiment_ids: vec!["zzzz".into()],
        }];
        assert!(store.propose_import(&id, Some(unknown), &agent()).is_err());
        // Discarding writes nothing.
        store
            .resolve_decision(
                &card.id,
                &DecisionChoice {
                    option: Some("discard".into()),
                    ..Default::default()
                },
                &Actor::User,
            )
            .unwrap();
        assert!(store.list_epics(&id).unwrap().is_empty());
    }
}
