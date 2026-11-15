//! The board's scheduler inside `orx up`: it starts ready tickets on free GPU
//! slots, each in a chat of its own, notes when their runs end, and repairs
//! tickets whose chat was lost across a restart.

use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast;

use super::{bad_request, not_found, ApiResult};
use super::{new_chat_session, AppState, CreateChatSessionReq};
use crate::board::{brief, scheduler, ProposedBy, Ticket, TicketStatus};
use crate::board::{Actor, Decision, TicketAction};
use crate::engine::bus::RunEvent;
use crate::error::{anyhow, Result};
use crate::local::autonomy::Autonomy;
use crate::local::chat::{OrxDisplay, OrxKind, OrxSource, TurnOverrides};
use crate::store::board::{BoardSettingsUpdate, DecisionChoice};
use crate::store::{now_ms, Store};
use axum::extract::Path;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

/// How long a dispatched ticket may sit without a chat before it is started again.
const UNBOUND_GRACE_MS: i64 = 2 * 60 * 1000;
const DEFAULT_MAX_SESSIONS: usize = 8;

pub(super) fn spawn(state: AppState, mut runs: broadcast::Receiver<RunEvent>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(crate::config::paced(Duration::from_secs(5)));
        loop {
            let mut ended = Vec::new();
            tokio::select! {
                event = runs.recv() => match event {
                    Ok(RunEvent::Changed { run_id, phase, .. }) if phase.is_terminal() => {
                        ended.push((run_id, phase.as_str().to_string()));
                    }
                    // A lagged receiver missed events; the reconcile pass below rereads the store.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                    _ => continue,
                },
                _ = tick.tick() => {}
            }
            if state
                .data_dir_move_in_progress
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                continue;
            }
            let _gate = state.data_dir_gate.lock().await;
            if let Err(error) = pass(&state, &ended).await {
                eprintln!("orx up: board scheduler: {error:#}");
            }
        }
    });
}

async fn pass(state: &AppState, ended: &[(String, String)]) -> Result<()> {
    // The store is not `Send`: every use of it finishes before the next await.
    // While orx is still detecting agents, a start would fail for no real reason.
    let agents_settling = agents_settling(state).await;
    let (started, messages, stopping) = {
        let store = Store::open()?;
        for (run_id, phase) in ended {
            note_run_end(&store, run_id, phase)?;
        }
        reconcile(&store)?;
        for epic in store.active_epics()? {
            store.check_epic_budget(&epic)?;
        }
        let messages = store.undelivered_ticket_messages()?;
        let stopping = store
            .tickets_to_stop()?
            .into_iter()
            .map(|ticket| Ok((store.ticket_live_runs(&ticket)?, ticket)))
            .collect::<Result<Vec<_>>>()?;
        let started = if agents_settling {
            Vec::new()
        } else {
            dispatch(&store)?
        };
        (started, messages, stopping)
    };
    for (runs, ticket) in stopping {
        let Some(session) = &ticket.session_id else {
            continue;
        };
        // Called off means its compute stops too, not only the chat.
        for run in &runs {
            let reason = format!("the user {} {}", ticket.status, ticket.key);
            if let Err(error) = super::api_v2::cancel_run(run, &reason).await {
                eprintln!(
                    "orx up: could not cancel {}'s run {}: {error:#}",
                    ticket.key, run.id
                );
            }
        }
        // The user called the ticket off: its chat stops and drops anything queued.
        if let Err(error) = state.chat.interrupt_by_user(session).await {
            eprintln!("orx up: could not stop {}'s chat: {error:#}", ticket.key);
            continue;
        }
        let cancelled: Vec<&str> = runs.iter().map(|run| run.id.as_str()).collect();
        Store::open()?.note_ticket_once(
            &ticket,
            "chat_stopped",
            &json!({"sessionId": session, "cancelledRuns": cancelled}),
        )?;
    }
    for (event, ticket) in messages {
        if let Err(error) = deliver(state, &event, &ticket).await {
            eprintln!(
                "orx up: could not tell {}'s chat about the user's {}: {error:#}",
                ticket.key, event.kind
            );
        }
    }
    for ticket in started {
        if let Err(error) = start_executor(state, &ticket).await {
            eprintln!("orx up: could not start {}: {error:#}", ticket.key);
            Store::open()?
                .requeue_ticket(&ticket.id, &format!("its chat could not start: {error}"))?;
        }
    }
    Ok(())
}

/// Sends the user's answer, challenge or comment to the ticket's chat; queued if it is busy.
async fn deliver(
    state: &AppState,
    event: &crate::board::BoardEvent,
    ticket: &Ticket,
) -> Result<()> {
    let Some(session) = ticket.session_id.clone() else {
        return Ok(());
    };
    if let Some(text) = brief::user_message(ticket, &event.kind, &event.detail) {
        state
            .chat
            .send_message(
                &session,
                text.clone(),
                TurnOverrides::default(),
                Vec::new(),
                Vec::new(),
                Some(format!("ticket_{}_event_{}", ticket.id, event.id)),
                Some(OrxDisplay {
                    text,
                    source: OrxSource {
                        kind: OrxKind::Message,
                        from: None,
                        status: None,
                    },
                }),
            )
            .await?;
    }
    Store::open()?.mark_ticket_message_delivered(ticket, event.id, &session)
}

async fn agents_settling(state: &AppState) -> bool {
    super::harnesses_payload(state, &super::HarnessQuery::default()).await["harnesses"]
        .as_array()
        .is_none_or(|harnesses| {
            harnesses
                .iter()
                .any(|harness| harness["catalogPending"].as_bool() == Some(true))
        })
}

fn note_run_end(store: &Store, run_id: &str, phase: &str) -> Result<()> {
    let Some(run) = store.get_run(run_id)? else {
        return Ok(());
    };
    if let Some(ticket) = store.ticket_for_experiment(&run.experiment_id)? {
        store.record_ticket_run_end(&ticket, run_id, phase)?;
    }
    Ok(())
}

/// Brings running tickets back in line with their chats and runs.
fn reconcile(store: &Store) -> Result<()> {
    let now = now_ms();
    for ticket in store.tickets_in(&[TicketStatus::Running, TicketStatus::Waiting])? {
        if let Some(experiment) = &ticket.experiment_id {
            if let Some(run) = store.latest_run_for_experiment(experiment)? {
                if run.ended_at.is_some() {
                    store.record_ticket_run_end(&ticket, &run.id, &run.status)?;
                }
            }
        }
        overrun(store, &ticket, now)?;
        if ticket.status != TicketStatus::Running {
            continue;
        }
        let lost = match &ticket.session_id {
            Some(session) => store
                .get_chat_session(session)?
                .is_none()
                .then_some("its chat was deleted"),
            None => (now - ticket.started_at.unwrap_or(now) > UNBOUND_GRACE_MS)
                .then_some("its chat never started"),
        };
        match lost {
            // The user's own chat asked for it; another chat must not pick it up.
            Some(reason) if ticket.proposed_by == ProposedBy::UserChat => {
                store.move_ticket(
                    &ticket.id,
                    TicketAction::Block,
                    &Actor::System(None),
                    json!({"reason": reason}),
                )?;
            }
            Some(reason) => {
                store.requeue_ticket(&ticket.id, reason)?;
            }
            None => {}
        }
    }
    Ok(())
}

fn overrun(store: &Store, ticket: &Ticket, now: i64) -> Result<()> {
    let (Some(started), Some(estimate)) = (ticket.started_at, ticket.est_minutes) else {
        return Ok(());
    };
    let factor = ticket
        .epic_id
        .as_deref()
        .and_then(|epic| store.epic(epic).ok().flatten())
        .map(|epic| epic.rules.overrun_factor)
        .unwrap_or(1.5);
    let elapsed_min = (now - started) as f64 / 60_000.0;
    if elapsed_min > estimate as f64 * factor {
        store.note_ticket_once(
            ticket,
            "overrun",
            &json!({"estimatedMinutes": estimate, "elapsedMinutes": elapsed_min.round(), "factor": factor}),
        )?;
    }
    Ok(())
}

/// Moves the tickets the scheduler picks to running; their chats start after.
fn dispatch(store: &Store) -> Result<Vec<Ticket>> {
    let live = store
        .tickets_in(&[TicketStatus::Running, TicketStatus::Waiting])?
        .iter()
        .filter(|ticket| ticket.session_id.is_some())
        .count();
    let max_sessions = store
        .board_setting("maxSessions")?
        .as_u64()
        .map_or(DEFAULT_MAX_SESSIONS, |max| max as usize);
    scheduler::plan(
        &store.dispatch_candidates()?,
        &store.machine_slots()?,
        live,
        max_sessions,
    )
    .into_iter()
    .map(|start| store.dispatch_ticket(&start.ticket_id, start.machine_id.as_deref()))
    .collect()
}

/// Opens the ticket's chat with the user's New Chat agent (or the ticket's own) and sends its task.
async fn start_executor(state: &AppState, ticket: &Ticket) -> Result<()> {
    let _admission = state
        .project_lifecycle
        .admit(&ticket.project_id)
        .ok_or_else(|| anyhow!("its project is being deleted"))?;
    let mut session = new_chat_session(
        state,
        CreateChatSessionReq {
            profile: None,
            project_id: ticket.project_id.clone(),
            harness: ticket.harness.clone(),
            model: None,
            service_tier: None,
            permission_mode: None,
            plan_mode: false,
            reasoning_level: None,
            autonomy: Some(Autonomy::Agentic),
        },
    )
    .await
    .map_err(|error| anyhow!("{}", error.1))?;
    session.title = Some(format!("{} · {}", ticket.key, ticket.title));
    session.title_source = Some("user".into());
    // Agentic: the ticket was approved, so its chat runs without asking before each launch.
    session.autonomy = Some(Autonomy::Agentic.id().to_string());
    let task = {
        let store = Store::open()?;
        store.create_chat_session(&session)?;
        store.bind_ticket_session(&ticket.id, &session.id)?;
        let epic = ticket
            .epic_id
            .as_deref()
            .map(|id| store.require_epic(id))
            .transpose()?;
        let criteria = match &epic {
            Some(epic) if epic.criteria_version > 0 => {
                store.criteria(&epic.id, epic.criteria_version)?
            }
            _ => None,
        };
        let machine = ticket
            .machine_id
            .as_deref()
            .map(|id| store.machine(id))
            .transpose()?
            .flatten();
        brief::executor_task(ticket, epic.as_ref(), criteria.as_ref(), machine.as_ref())
    };
    state.chat.emit_session(Some(session.clone())).await;
    let sent = state
        .chat
        .send_message(
            &session.id,
            task,
            TurnOverrides::default(),
            Vec::new(),
            Vec::new(),
            Some(format!("ticket_{}_{}", ticket.id, ticket.repair_count)),
            Some(OrxDisplay {
                text: format!("Ticket {}: {}", ticket.key, ticket.title),
                source: OrxSource {
                    kind: OrxKind::Task,
                    from: None,
                    status: None,
                },
            }),
        )
        .await;
    if let Err(error) = sent {
        if let Err(cleanup) = state.chat.delete_session(&session.id).await {
            eprintln!(
                "orx up: could not remove {}'s unstarted chat {}: {cleanup:#}",
                ticket.key, session.id
            );
        }
        return Err(error);
    }
    Ok(())
}

// The dashboard's board API. The page acts for the user: it is the one place
// approvals, acceptances and answers come from outside the CLI.

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/board/inbox", get(inbox))
        .route(
            "/api/board/projects/{id}",
            get(project_board).post(update_project),
        )
        .route("/api/board/tickets/{id}", get(ticket_detail))
        .route("/api/board/tickets/{id}/comment", post(comment))
        .route("/api/board/tickets/{id}/move", post(move_ticket))
        .route("/api/board/decisions/{id}/resolve", post(resolve))
        .route("/api/board/machines", get(list_machines).post(add_machine))
        .route("/api/board/machines/{name}", delete(remove_machine))
}

/// Board refusals are the user's to act on, so they come back as 400s with the store's message.
fn refused(error: crate::error::Error) -> super::ApiError {
    bad_request(error)
}

fn ticket_summary(ticket: &Ticket) -> Value {
    json!({
        "id": ticket.id, "key": ticket.key, "title": ticket.title, "kind": ticket.kind,
        "status": ticket.status, "outcome": ticket.outcome, "conclusion": ticket.conclusion,
        "hypothesis": ticket.hypothesis, "estMinutes": ticket.est_minutes, "needsGpus": ticket.needs_gpus,
        "startedAt": ticket.started_at, "sessionId": ticket.session_id, "projectId": ticket.project_id,
        "experimentId": ticket.experiment_id, "result": ticket.result,
    })
}

fn decision_json(store: &Store, decision: &Decision) -> crate::error::Result<Value> {
    let tickets = decision
        .ticket_ids
        .iter()
        .filter_map(|id| store.ticket(id).transpose())
        .collect::<crate::error::Result<Vec<_>>>()?;
    let epic = decision
        .epic_id
        .as_deref()
        .map(|id| store.epic(id))
        .transpose()?
        .flatten();
    let criteria = match (&decision.kind, &epic) {
        (crate::board::DecisionKind::ApproveCriteria, Some(epic)) => decision.payload["version"]
            .as_i64()
            .map(|version| store.criteria(&epic.id, version))
            .transpose()?
            .flatten(),
        _ => None,
    };
    let project = store.get_local_project(&decision.project_id)?;
    Ok(json!({
        "decision": decision,
        "projectName": project.map(|project| project.name),
        "epic": epic.map(|epic| json!({"id": epic.id, "key": epic.key, "title": epic.title, "question": epic.question})),
        "criteria": criteria,
        "tickets": tickets.iter().map(ticket_summary).collect::<Vec<_>>(),
    }))
}

async fn inbox() -> ApiResult {
    let store = Store::open()?;
    let decisions = store
        .open_decisions(None)?
        .iter()
        .map(|decision| decision_json(&store, decision))
        .collect::<crate::error::Result<Vec<_>>>()?;
    let running = store
        .tickets_in(&[TicketStatus::Running, TicketStatus::Waiting])?
        .iter()
        .map(|ticket| {
            let mut row = ticket_summary(ticket);
            row["projectName"] = json!(store
                .get_local_project(&ticket.project_id)?
                .map(|project| project.name));
            Ok(row)
        })
        .collect::<crate::error::Result<Vec<_>>>()?;
    let slots = store.machine_slots()?;
    let machines = store
        .list_machines()?
        .into_iter()
        .map(|machine| {
            let in_use = slots
                .iter()
                .find(|slot| slot.machine_id == machine.id)
                .map_or(0, |slot| slot.in_use);
            json!({"machine": machine, "inUse": in_use})
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"decisions": decisions, "running": running, "machines": machines}),
    ))
}

async fn project_board(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    store
        .get_local_project(&id)?
        .ok_or_else(|| not_found("project"))?;
    let Some(board) = store.board_project(&id)? else {
        return Ok(Json(json!({"board": null, "epics": [], "tickets": []})));
    };
    let epics = store
        .list_epics(&id)?
        .into_iter()
        .map(|epic| {
            let criteria = store.latest_criteria(&epic.id)?;
            Ok(json!({"epic": epic, "criteria": criteria}))
        })
        .collect::<crate::error::Result<Vec<_>>>()?;
    let tickets = store.list_tickets(&id)?;
    Ok(Json(
        json!({"board": board, "epics": epics, "tickets": tickets}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectBoardReq {
    enabled: Option<bool>,
    priority: Option<i64>,
    confidential: Option<bool>,
}

async fn update_project(Path(id): Path<String>, Json(req): Json<ProjectBoardReq>) -> ApiResult {
    let board = Store::open()?
        .update_board_project(
            &id,
            &BoardSettingsUpdate {
                enabled: req.enabled,
                key_prefix: None,
                priority: req.priority,
                confidential: req.confidential,
                deadline_ms: None,
            },
        )
        .map_err(refused)?;
    Ok(Json(json!({"board": board})))
}

async fn ticket_detail(Path(id): Path<String>) -> ApiResult {
    let store = Store::open()?;
    let ticket = store.ticket(&id)?.ok_or_else(|| not_found("ticket"))?;
    let events = store.board_events(&ticket.id)?;
    let epic = ticket
        .epic_id
        .as_deref()
        .map(|id| store.epic(id))
        .transpose()?
        .flatten();
    Ok(Json(
        json!({"ticket": ticket, "events": events, "epic": epic}),
    ))
}

#[derive(Deserialize)]
struct CommentReq {
    text: String,
}

async fn comment(Path(id): Path<String>, Json(req): Json<CommentReq>) -> ApiResult {
    Store::open()?
        .comment_on_ticket(&id, &req.text, &Actor::User)
        .map_err(refused)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct MoveReq {
    action: String,
    note: Option<String>,
}

async fn move_ticket(Path(id): Path<String>, Json(req): Json<MoveReq>) -> ApiResult {
    let action = match req.action.as_str() {
        "approve" => TicketAction::Approve,
        "reject" => TicketAction::Reject,
        "accept" => TicketAction::Accept,
        "challenge" => TicketAction::Challenge,
        "unblock" => TicketAction::Unblock,
        "cancel" => TicketAction::Cancel,
        other => return Err(bad_request(format!("`{other}` is not a ticket action"))),
    };
    let ticket = Store::open()?
        .move_ticket(&id, action, &Actor::User, json!({"note": req.note}))
        .map_err(refused)?;
    Ok(Json(json!({"ticket": ticket})))
}

#[derive(Deserialize)]
struct ResolveReq {
    selected: Option<Vec<String>>,
    option: Option<String>,
    text: Option<String>,
}

async fn resolve(Path(id): Path<String>, Json(req): Json<ResolveReq>) -> ApiResult {
    let decision = Store::open()?
        .resolve_decision(
            &id,
            &DecisionChoice {
                selected: req.selected,
                option: req.option,
                text: req.text,
            },
            &Actor::User,
        )
        .map_err(refused)?;
    Ok(Json(json!({"decision": decision})))
}

async fn list_machines() -> ApiResult {
    Ok(Json(json!({"machines": Store::open()?.list_machines()?})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MachineReq {
    name: String,
    backend: String,
    gpu_slots: i64,
    own: bool,
}

async fn add_machine(Json(req): Json<MachineReq>) -> ApiResult {
    let machine = Store::open()?
        .upsert_machine(&req.name, &req.backend, req.gpu_slots, req.own)
        .map_err(refused)?;
    Ok(Json(json!({"machine": machine})))
}

async fn remove_machine(Path(name): Path<String>) -> ApiResult {
    Store::open()?.remove_machine(&name).map_err(refused)?;
    Ok(Json(json!({"ok": true})))
}
