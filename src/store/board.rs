//! Board records: epics, tickets, the user's decisions and the activity log.
//!
//! Every write that changes a ticket or decision appends to `board_events`
//! in the same transaction, so the log is the dated record of who decided what.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{json, Value};

use super::{now_ms, Store};
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
            "SELECT {EPIC_COLUMNS} FROM board_epics WHERE project_id = ?1 ORDER BY priority, created_at"
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
        let dedupe = format!("approve:{}:{card}", new.project_id);
        let open: Option<(String, String)> = tx
            .query_row(
                "SELECT id, ticket_ids_json FROM board_decisions WHERE dedupe_key = ?1 AND status = 'open'",
                [&dedupe],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match open {
            Some((decision_id, ids)) => {
                let mut ids: Vec<String> = serde_json::from_str(&ids)?;
                ids.push(id.clone());
                tx.execute(
                    "UPDATE board_decisions SET ticket_ids_json = ?2 WHERE id = ?1",
                    params![decision_id, serde_json::to_string(&ids)?],
                )?;
            }
            None => {
                // A closed card for this batch keeps its key; later proposals get a fresh one.
                let dedupe = match tx
                    .query_row(
                        "SELECT 1 FROM board_decisions WHERE dedupe_key = ?1",
                        [&dedupe],
                        |_| Ok(()),
                    )
                    .optional()?
                {
                    Some(()) => format!("{dedupe}:{id}"),
                    None => dedupe,
                };
                let epic_id = new
                    .epic_id
                    .as_ref()
                    .map(|epic| self.require_epic(epic).map(|epic| epic.id))
                    .transpose()?;
                Self::insert_decision(
                    &tx,
                    &new.project_id,
                    epic_id.as_deref(),
                    DecisionKind::ApproveBatch,
                    std::slice::from_ref(&id),
                    &json!({"batch": new.batch_id}),
                    &dedupe,
                )?;
            }
        }
        tx.commit()?;
        self.require_ticket(&id)
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
                gaps.push("an estimated duration (`--est`)".to_string());
            }
            if ticket.validation.is_none() {
                gaps.push("its validation metrics and logs (`--validation-file`)".to_string());
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
        tx.execute(
            "UPDATE board_tickets SET status = ?2, updated_at = ?3,
                 started_at = CASE WHEN ?4 THEN ?3 ELSE started_at END,
                 finished_at = CASE WHEN ?5 THEN ?3 ELSE finished_at END
             WHERE id = ?1",
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
                ) && ticket.finished_at.is_none()
            ],
        )?;
        Self::append_board_event(
            &tx,
            &ticket.project_id,
            &ticket.id,
            actor,
            action.name(),
            &json!({"from": ticket.status, "to": to, "detail": detail}),
        )?;
        Self::settle_approval_cards(&tx, &ticket.id, now)?;
        tx.commit()?;
        self.require_ticket(&ticket.id)
    }

    /// Closes approval cards whose tickets have all left `proposed`.
    fn settle_approval_cards(conn: &Connection, ticket_id: &str, now: i64) -> Result<()> {
        let mut statement = conn.prepare(
            "SELECT d.id, d.ticket_ids_json FROM board_decisions d, json_each(d.ticket_ids_json) j
             WHERE d.kind = 'approve_batch' AND d.status = 'open' AND j.value = ?1",
        )?;
        let cards: Vec<(String, String)> = statement
            .query_map([ticket_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (card, ids) in cards {
            let ids: Vec<String> = serde_json::from_str(&ids)?;
            let mut approved = Vec::new();
            let mut declined = Vec::new();
            let mut pending = false;
            for id in &ids {
                let status: String = conn.query_row(
                    "SELECT status FROM board_tickets WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )?;
                match status.as_str() {
                    "proposed" => pending = true,
                    "rejected" | "cancelled" => declined.push(id.clone()),
                    _ => approved.push(id.clone()),
                }
            }
            if !pending {
                conn.execute(
                    "UPDATE board_decisions SET status = 'resolved', resolved_at = ?2, resolution_json = ?3
                     WHERE id = ?1",
                    params![card, now, json!({"approved": approved, "declined": declined}).to_string()],
                )?;
            }
        }
        Ok(())
    }

    /// The executor hands its result in: the ticket goes to review with its outcome.
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
        self.require_ticket(&ticket.id)
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
                let selected = choice.selected.clone().unwrap_or_else(|| decision.ticket_ids.clone());
                let selected: Vec<String> = selected
                    .iter()
                    .map(|key| self.require_ticket(key).map(|ticket| ticket.id))
                    .collect::<Result<_>>()?;
                if let Some(stray) = selected.iter().find(|id| !decision.ticket_ids.contains(id)) {
                    bail!("Ticket {stray} is not on this card.");
                }
                // Check every selected ticket first so one gap does not leave half a batch approved.
                for ticket_id in &selected {
                    let ticket = self.require_ticket(ticket_id)?;
                    let gaps = self.ready_gaps(&ticket)?;
                    if ticket.status == TicketStatus::Proposed && !gaps.is_empty() {
                        bail!("{} cannot be approved yet: it still needs {}.", ticket.key, gaps.join(" and "));
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
                    self.move_ticket(ticket_id, action, actor, json!({"decision": decision.id, "note": choice.text}))?;
                }
            }
            DecisionKind::Question => {
                let answer = choice.option.clone().or_else(|| choice.text.clone()).ok_or_else(|| {
                    anyhow!("Answer with `--option <one of the options>` or `--text \"…\"`.")
                })?;
                if let (Some(option), Some(options)) = (&choice.option, decision.payload["options"].as_array()) {
                    if !options.is_empty() && !options.iter().any(|known| known.as_str() == Some(option)) {
                        bail!("`{option}` is not one of this question's options.");
                    }
                }
                for ticket_id in &decision.ticket_ids {
                    self.move_ticket(ticket_id, TicketAction::Answer, actor, json!({"decision": decision.id, "answer": answer}))?;
                }
                self.close_decision(&decision, choice)?;
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
            other => bail!("`{other}` cards are not decided from the CLI yet; open the board in the dashboard."),
        }
        self.require_decision(&decision.id)
    }

    fn close_decision(&self, decision: &Decision, choice: &DecisionChoice) -> Result<()> {
        self.conn.execute(
            "UPDATE board_decisions SET status = 'resolved', resolved_at = ?2, resolution_json = ?3
             WHERE id = ?1 AND status = 'open'",
            params![
                decision.id,
                now_ms(),
                json!({"selected": choice.selected, "option": choice.option, "text": choice.text})
                    .to_string()
            ],
        )?;
        Ok(())
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
}
