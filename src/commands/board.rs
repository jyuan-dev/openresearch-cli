//! The research board from the terminal.
//!
//!   orx board                        open decisions across every project (the inbox)
//!   orx board view <projectId>       a project's epics and tickets by status
//!   orx board enable <projectId>     start or configure a project's board
//!   orx board machines …             the machines and GPU slots tickets run on
//!   orx board decide <decisionId>    answer a decision card
//!   orx epic …                       research questions and their answer criteria
//!   orx ticket …                     propose, decide, report and comment on tickets
//!
//! Agents propose, ask and report; only the user approves, accepts and answers.

use std::io::Read;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Value};

use crate::board::{Actor, EpicStatus, Outcome, Ticket, TicketAction, TicketKind, TicketStatus};
use crate::error::{anyhow, bail, Context, Result};
use crate::output::print_table;
use crate::store::board::{BoardSettingsUpdate, DecisionChoice, NewEpic, NewTicket};
use crate::store::Store;

#[derive(Args, Debug)]
pub struct BoardArgs {
    #[command(subcommand)]
    pub command: Option<BoardCommand>,
    /// Emit JSON.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum BoardCommand {
    /// Show a project's epics and tickets, grouped by status.
    View { project_id: String },
    /// Start a project's board, or change its settings.
    Enable {
        project_id: String,
        /// Ticket key prefix, like `SJ` for SJ-001. Defaults to the project's initials.
        #[arg(long)]
        prefix: Option<String>,
        /// Order among projects sharing the GPUs; lower runs first.
        #[arg(long)]
        priority: Option<i64>,
        /// Confidential projects run only on your own machines and cannot be published.
        #[arg(long)]
        confidential: Option<OnOff>,
        /// Turn the board off again (tickets are kept).
        #[arg(long)]
        disable: bool,
    },
    /// List, add or remove the machines tickets are scheduled on.
    Machines {
        #[command(subcommand)]
        command: Option<MachineCommand>,
    },
    /// Answer a decision card from `orx board`.
    Decide {
        decision_id: String,
        /// On a batch card: the tickets to approve; the rest are declined. Default: all.
        #[arg(long = "select", value_name = "TICKET")]
        select: Vec<String>,
        /// On a question card: the option you pick.
        #[arg(long)]
        option: Option<String>,
        /// A free-text answer or note.
        #[arg(long)]
        text: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MachineCommand {
    /// Register a machine, or update one with the same name.
    Add {
        name: String,
        /// `local`, or `ssh:<alias>` for a host in your SSH config.
        #[arg(long)]
        backend: String,
        /// How many GPUs tickets may hold on it at once.
        #[arg(long)]
        gpus: i64,
        /// Not your own hardware: confidential projects will not run on it.
        #[arg(long)]
        not_own: bool,
    },
    Remove {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Args, Debug)]
pub struct EpicArgs {
    #[command(subcommand)]
    pub command: EpicCommand,
    /// Emit JSON.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum EpicCommand {
    /// Open a research question on a project's board.
    Add {
        project_id: String,
        #[arg(long)]
        title: String,
        /// The research question this epic answers.
        #[arg(long)]
        question: String,
        #[arg(long, default_value_t = 100)]
        priority: i64,
        /// GPU-hour budget for the whole epic.
        #[arg(long)]
        budget_gpu_hours: Option<f64>,
    },
    /// Show an epic, its answer criteria and its tickets.
    Show { epic: String },
    /// Propose the next version of the epic's answer criteria; the user approves it.
    Criteria {
        epic: String,
        /// What result answers the question, in prose.
        #[arg(long, conflicts_with = "stdin")]
        body: Option<String>,
        /// Read the body from stdin.
        #[arg(long)]
        stdin: bool,
        /// JSON file of machine checks (metric, minSeeds, noiseFloor, …) overriding the defaults.
        #[arg(long)]
        checks_file: Option<String>,
    },
    /// Approve answer criteria (the latest version unless `--version`).
    Approve {
        epic: String,
        #[arg(long)]
        version: Option<i64>,
    },
    /// Set the epic's standing rules from a JSON file (user only); unnamed rules keep their defaults.
    Rules {
        epic: String,
        #[arg(long)]
        file: String,
    },
    /// Close an answered epic with its conclusion.
    Close {
        epic: String,
        #[arg(long)]
        conclusion: String,
    },
    /// Drop an epic that will not be pursued.
    Drop {
        epic: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct TicketArgs {
    #[command(subcommand)]
    pub command: TicketCommand,
    /// Emit JSON.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct TicketSpec {
    /// The epic it belongs to (`SJ-E3`). Without one, give `--project` for an unplanned ticket.
    #[arg(long, required_unless_present = "project")]
    epic: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    title: String,
    #[arg(long, value_enum, default_value = "experiment")]
    kind: KindArg,
    #[arg(long)]
    hypothesis: Option<String>,
    /// The concrete change to make on the experiment's branch.
    #[arg(long = "change")]
    change_spec: Option<String>,
    /// Experiment to branch from.
    #[arg(long = "base")]
    base_experiment_id: Option<String>,
    /// GPUs the run needs while it holds a slot.
    #[arg(long, default_value_t = 0)]
    gpus: i64,
    /// Estimated duration, like `90m`, `2h` or `1h30m`.
    #[arg(long)]
    est: Option<String>,
    /// JSON file: validation metrics, how often they are logged, and where training logs go.
    #[arg(long)]
    validation_file: Option<String>,
    /// Groups the options of one decision (a round) onto one approval card.
    #[arg(long = "batch")]
    batch_id: Option<String>,
    /// The agent to run it with; defaults to the project's.
    #[arg(long)]
    harness: Option<String>,
    /// A refuting result would overturn the epic's core hypothesis: always reviewed on its own.
    #[arg(long)]
    core: bool,
    #[arg(long, default_value_t = 100)]
    priority: i64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum KindArg {
    Experiment,
    Analysis,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutcomeArg {
    Supports,
    Refutes,
    Inconclusive,
}

#[derive(Subcommand, Debug)]
pub enum TicketCommand {
    /// Propose a ticket; it waits on an approval card for the user.
    Propose(Box<TicketSpec>),
    /// Add a ticket and approve it at once (user only).
    Add(Box<TicketSpec>),
    /// Show a ticket and its activity.
    Show { ticket: String },
    /// Approve a proposed ticket (user only).
    Approve { ticket: String },
    /// Decline a proposed or blocked ticket (user only).
    Reject {
        ticket: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Accept a reported result (user only).
    Accept { ticket: String },
    /// Dispute a reported result; the agent picks the ticket back up (user only).
    Challenge {
        ticket: String,
        #[arg(long)]
        note: String,
    },
    /// Send a blocked ticket back to ready (user only).
    Unblock { ticket: String },
    /// Stop a ticket for good (user only).
    Cancel {
        ticket: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Ask the user a question; only this ticket waits for the answer.
    Ask {
        ticket: String,
        #[arg(long)]
        question: String,
        /// 2–4 options the user can pick with one tap.
        #[arg(long = "option")]
        options: Vec<String>,
        /// The option you recommend.
        #[arg(long)]
        recommend: Option<String>,
    },
    /// Hand in the ticket's result for review.
    Report {
        ticket: String,
        #[arg(long, value_enum)]
        outcome: OutcomeArg,
        #[arg(long)]
        conclusion: String,
        /// JSON file with per-seed metrics, the baseline's value and anything the checks need.
        #[arg(long)]
        result_file: Option<String>,
    },
    /// Stop work that cannot go on without the user.
    Block {
        ticket: String,
        #[arg(long)]
        reason: String,
    },
    /// Add a note to the ticket's activity.
    Comment { ticket: String, text: String },
    /// Mark a done ticket's result as no longer trustworthy (user only).
    Taint {
        ticket: String,
        #[arg(long)]
        reason: String,
    },
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn read_json_file(path: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    serde_json::from_str(&text).with_context(|| format!("{path} is not valid JSON"))
}

/// Minutes in `90m`, `2h`, `1h30m` or a bare number of minutes.
pub fn parse_minutes(text: &str) -> Result<i64> {
    let text = text.trim().to_ascii_lowercase();
    let invalid = || anyhow!("`{text}` is not a duration; write it like `90m`, `2h` or `1h30m`.");
    if let Ok(minutes) = text.parse::<i64>() {
        return if minutes > 0 {
            Ok(minutes)
        } else {
            Err(invalid())
        };
    }
    let (hours, rest) = match text.split_once('h') {
        Some((hours, rest)) => (hours.parse::<i64>().map_err(|_| invalid())?, rest),
        None => (0, text.as_str()),
    };
    let minutes = match rest.strip_suffix('m') {
        Some(minutes) => minutes.parse::<i64>().map_err(|_| invalid())?,
        None if rest.is_empty() => 0,
        None => return Err(invalid()),
    };
    let total = hours * 60 + minutes;
    if total <= 0 {
        return Err(invalid());
    }
    Ok(total)
}

fn ago(ms: i64) -> String {
    crate::output::format_duration((crate::store::now_ms() - ms) / 1000)
}

pub fn board(args: BoardArgs) -> Result<()> {
    let actor = Actor::from_environment();
    match args.command {
        None => inbox(args.json),
        Some(BoardCommand::View { project_id }) => view(&project_id, args.json),
        Some(BoardCommand::Enable {
            project_id,
            prefix,
            priority,
            confidential,
            disable,
        }) => {
            if actor != Actor::User {
                bail!("Only the user changes board settings.");
            }
            let board = Store::open()?.update_board_project(
                &project_id,
                &BoardSettingsUpdate {
                    enabled: Some(!disable),
                    key_prefix: prefix,
                    priority,
                    confidential: confidential.map(|value| matches!(value, OnOff::On)),
                    deadline_ms: None,
                },
            )?;
            if args.json {
                return print_json(&board);
            }
            println!(
                "Board {} for {} (prefix {}, priority {}, {}).",
                if board.enabled { "on" } else { "off" },
                board.project_id,
                board.key_prefix,
                board.priority,
                if board.confidential {
                    "confidential: own machines only, publishing blocked"
                } else {
                    "not confidential"
                }
            );
            Ok(())
        }
        Some(BoardCommand::Machines { command }) => machines(command, &actor, args.json),
        Some(BoardCommand::Decide {
            decision_id,
            select,
            option,
            text,
        }) => {
            let decision = Store::open()?.resolve_decision(
                &decision_id,
                &DecisionChoice {
                    selected: (!select.is_empty()).then_some(select),
                    option,
                    text,
                },
                &actor,
            )?;
            if args.json {
                return print_json(&decision);
            }
            println!("Decided ({}).", decision.kind);
            Ok(())
        }
    }
}

fn inbox(json: bool) -> Result<()> {
    let store = Store::open_for_reading()?;
    let decisions = store.open_decisions(None)?;
    if json {
        return print_json(&decisions);
    }
    if decisions.is_empty() {
        println!("Nothing waits for you.");
        return Ok(());
    }
    let rows = decisions
        .iter()
        .map(|decision| {
            let tickets = decision
                .ticket_ids
                .iter()
                .filter_map(|id| store.ticket(id).ok().flatten().map(|ticket| ticket.key))
                .collect::<Vec<_>>()
                .join(" ");
            let summary = match decision.kind {
                crate::board::DecisionKind::Question => decision.payload["question"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                crate::board::DecisionKind::ApproveCriteria => {
                    format!("criteria v{}", decision.payload["version"])
                }
                _ => String::new(),
            };
            vec![
                decision.id.clone(),
                decision.kind.to_string(),
                tickets,
                summary,
                ago(decision.created_at),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["DECISION", "KIND", "TICKETS", "ABOUT", "AGE"], &rows);
    Ok(())
}

fn view(project_id: &str, json: bool) -> Result<()> {
    let store = Store::open_for_reading()?;
    let board = store.board_project(project_id)?.ok_or_else(|| {
        anyhow!("Project {project_id} has no board yet. Start one with `orx board enable {project_id}`.")
    })?;
    let epics = store.list_epics(project_id)?;
    let tickets = store.list_tickets(project_id)?;
    if json {
        return print_json(&json!({"board": board, "epics": epics, "tickets": tickets}));
    }
    println!("Epics");
    print_table(
        &["KEY", "STATUS", "CRITERIA", "TITLE"],
        &epics
            .iter()
            .map(|epic| {
                vec![
                    epic.key.clone(),
                    epic.status.to_string(),
                    if epic.criteria_version == 0 {
                        "not approved".into()
                    } else {
                        format!("v{}", epic.criteria_version)
                    },
                    epic.title.clone(),
                ]
            })
            .collect::<Vec<_>>(),
    );
    for status in TicketStatus::COLUMNS {
        let column: Vec<&Ticket> = tickets
            .iter()
            .filter(|ticket| ticket.status == status)
            .collect();
        if column.is_empty() {
            continue;
        }
        println!("\n{} ({})", status, column.len());
        print_table(
            &["KEY", "EPIC", "BATCH", "GPUS", "EST", "TITLE"],
            &column
                .iter()
                .map(|ticket| {
                    vec![
                        ticket.key.clone(),
                        ticket
                            .epic_id
                            .as_deref()
                            .and_then(|id| epics.iter().find(|epic| epic.id == id))
                            .map(|epic| epic.key.clone())
                            .unwrap_or_else(|| "unplanned".into()),
                        ticket.batch_id.clone().unwrap_or_default(),
                        ticket.needs_gpus.to_string(),
                        ticket
                            .est_minutes
                            .map(|m| format!("{m}m"))
                            .unwrap_or_default(),
                        format!(
                            "{}{}",
                            ticket.title,
                            if ticket.tainted { " [tainted]" } else { "" }
                        ),
                    ]
                })
                .collect::<Vec<_>>(),
        );
    }
    Ok(())
}

fn machines(command: Option<MachineCommand>, actor: &Actor, json: bool) -> Result<()> {
    let store = Store::open()?;
    match command {
        None => {}
        Some(_) if *actor != Actor::User => bail!("Only the user registers machines."),
        Some(MachineCommand::Add {
            name,
            backend,
            gpus,
            not_own,
        }) => {
            store.upsert_machine(&name, &backend, gpus, !not_own)?;
        }
        Some(MachineCommand::Remove { name }) => store.remove_machine(&name)?,
    }
    let machines = store.list_machines()?;
    if json {
        return print_json(&machines);
    }
    if machines.is_empty() {
        println!("No machines yet. Add one with `orx board machines add <name> --backend local --gpus <n>`.");
        return Ok(());
    }
    print_table(
        &["NAME", "BACKEND", "GPU SLOTS", "OWN"],
        &machines
            .iter()
            .map(|m| {
                vec![
                    m.name.clone(),
                    m.backend.clone(),
                    m.gpu_slots.to_string(),
                    if m.own { "yes" } else { "no" }.into(),
                ]
            })
            .collect::<Vec<_>>(),
    );
    Ok(())
}

pub fn epic(args: EpicArgs) -> Result<()> {
    let actor = Actor::from_environment();
    let store = match args.command {
        EpicCommand::Show { .. } => Store::open_for_reading()?,
        _ => Store::open()?,
    };
    let epic = match args.command {
        EpicCommand::Add {
            project_id,
            title,
            question,
            priority,
            budget_gpu_hours,
        } => store.create_epic(
            &NewEpic {
                project_id,
                title,
                question,
                priority,
                budget_gpu_hours,
                budget_deadline_ms: None,
            },
            &actor,
        )?,
        EpicCommand::Show { epic } => {
            let epic = store.require_epic(&epic)?;
            let criteria = store.latest_criteria(&epic.id)?;
            let tickets: Vec<Ticket> = store
                .list_tickets(&epic.project_id)?
                .into_iter()
                .filter(|ticket| ticket.epic_id.as_deref() == Some(epic.id.as_str()))
                .collect();
            if args.json {
                return print_json(
                    &json!({"epic": epic, "criteria": criteria, "tickets": tickets}),
                );
            }
            println!("{} {} [{}]", epic.key, epic.title, epic.status);
            println!("Question: {}", epic.question);
            match criteria {
                Some(criteria) => println!(
                    "Criteria v{}{}:\n{}",
                    criteria.version,
                    if criteria.approved_at.is_some() {
                        " (approved)"
                    } else {
                        " (waiting for approval)"
                    },
                    criteria.body_md
                ),
                None => println!(
                    "Criteria: none yet. Propose them with `orx epic criteria {}`.",
                    epic.key
                ),
            }
            for ticket in tickets {
                println!("  {}  {:<9}  {}", ticket.key, ticket.status, ticket.title);
            }
            return Ok(());
        }
        EpicCommand::Criteria {
            epic,
            body,
            stdin,
            checks_file,
        } => {
            let body = match (body, stdin) {
                (Some(body), _) => body,
                (None, true) => {
                    let mut body = String::new();
                    std::io::stdin().read_to_string(&mut body)?;
                    body
                }
                (None, false) => bail!("Give the criteria with `--body \"…\"` or `--stdin`."),
            };
            let checks = checks_file
                .as_deref()
                .map(read_json_file)
                .transpose()?
                .unwrap_or_else(|| json!({}));
            let criteria = store.propose_criteria(&epic, &body, &checks, &actor)?;
            if args.json {
                return print_json(&criteria);
            }
            println!(
                "Criteria v{} proposed for {epic}; it waits for the user's approval.",
                criteria.version
            );
            return Ok(());
        }
        EpicCommand::Approve { epic, version } => {
            let found = store.require_epic(&epic)?;
            let version = match version {
                Some(version) => version,
                None => {
                    store
                        .latest_criteria(&found.id)?
                        .ok_or_else(|| {
                            anyhow!(
                                "{} has no criteria yet. Propose them with `orx epic criteria {}`.",
                                found.key,
                                found.key
                            )
                        })?
                        .version
                }
            };
            store.approve_criteria(&found.id, version, &actor)?
        }
        EpicCommand::Rules { epic, file } => {
            let rules: crate::board::EpicRules = serde_json::from_value(read_json_file(&file)?)
                .with_context(|| format!("{file} does not hold epic rules"))?;
            store.set_epic_rules(&epic, &rules, &actor)?
        }
        EpicCommand::Close { epic, conclusion } => {
            store.set_epic_status(&epic, EpicStatus::Answered, Some(&conclusion), &actor)?
        }
        EpicCommand::Drop { epic, reason } => {
            store.set_epic_status(&epic, EpicStatus::Dropped, reason.as_deref(), &actor)?
        }
    };
    if args.json {
        return print_json(&epic);
    }
    println!("{} {} [{}]", epic.key, epic.title, epic.status);
    Ok(())
}

fn new_ticket(store: &Store, spec: TicketSpec) -> Result<NewTicket> {
    let (project_id, epic_id) = match (&spec.epic, &spec.project) {
        (Some(epic), _) => {
            let epic = store.require_epic(epic)?;
            (epic.project_id, Some(epic.id))
        }
        (None, Some(project)) => (project.clone(), None),
        (None, None) => {
            bail!("Give the ticket an `--epic`, or a `--project` for an unplanned ticket.")
        }
    };
    Ok(NewTicket {
        project_id,
        epic_id,
        kind: match spec.kind {
            KindArg::Experiment => TicketKind::Experiment,
            KindArg::Analysis => TicketKind::Analysis,
        },
        batch_id: spec.batch_id,
        title: spec.title,
        hypothesis: spec.hypothesis,
        change_spec: spec.change_spec,
        priority: spec.priority,
        base_experiment_id: spec.base_experiment_id,
        needs_gpus: spec.gpus,
        est_minutes: spec.est.as_deref().map(parse_minutes).transpose()?,
        validation: spec
            .validation_file
            .as_deref()
            .map(read_json_file)
            .transpose()?,
        harness: spec.harness,
        core_hypothesis: spec.core,
    })
}

pub fn ticket(args: TicketArgs) -> Result<()> {
    let actor = Actor::from_environment();
    let store = match args.command {
        TicketCommand::Show { .. } => Store::open_for_reading()?,
        _ => Store::open()?,
    };
    let ticket = match args.command {
        TicketCommand::Propose(spec) => {
            let new = new_ticket(&store, *spec)?;
            store.create_ticket(&new, &actor)?
        }
        TicketCommand::Add(spec) => {
            if actor != Actor::User {
                bail!("Agents propose tickets with `orx ticket propose`; only the user adds approved ones.");
            }
            let new = new_ticket(&store, *spec)?;
            let ticket = store.create_ticket(&new, &actor)?;
            store.move_ticket(&ticket.id, TicketAction::Approve, &actor, Value::Null)?
        }
        TicketCommand::Show { ticket } => {
            let ticket = store.require_ticket(&ticket)?;
            let events = store.board_events(&ticket.id)?;
            if args.json {
                return print_json(&json!({"ticket": ticket, "events": events}));
            }
            println!("{} {} [{}]", ticket.key, ticket.title, ticket.status);
            for (label, value) in [
                ("Hypothesis", ticket.hypothesis.as_deref()),
                ("Change", ticket.change_spec.as_deref()),
                ("Base", ticket.base_experiment_id.as_deref()),
                ("Experiment", ticket.experiment_id.as_deref()),
                ("Conclusion", ticket.conclusion.as_deref()),
                ("Tainted", ticket.tainted_reason.as_deref()),
            ] {
                if let Some(value) = value {
                    println!("{label}: {value}");
                }
            }
            if let Some(outcome) = ticket.outcome {
                println!("Outcome: {outcome}");
            }
            println!("Activity:");
            for event in events {
                println!(
                    "  {} ago  {:<16} {}",
                    ago(event.at),
                    event.kind,
                    event.actor
                );
            }
            return Ok(());
        }
        TicketCommand::Approve { ticket } => {
            store.move_ticket(&ticket, TicketAction::Approve, &actor, Value::Null)?
        }
        TicketCommand::Reject { ticket, note } => {
            store.move_ticket(&ticket, TicketAction::Reject, &actor, json!({"note": note}))?
        }
        TicketCommand::Accept { ticket } => {
            store.move_ticket(&ticket, TicketAction::Accept, &actor, Value::Null)?
        }
        TicketCommand::Challenge { ticket, note } => store.move_ticket(
            &ticket,
            TicketAction::Challenge,
            &actor,
            json!({"note": note}),
        )?,
        TicketCommand::Unblock { ticket } => {
            store.move_ticket(&ticket, TicketAction::Unblock, &actor, Value::Null)?
        }
        TicketCommand::Cancel { ticket, note } => {
            store.move_ticket(&ticket, TicketAction::Cancel, &actor, json!({"note": note}))?
        }
        TicketCommand::Ask {
            ticket,
            question,
            options,
            recommend,
        } => {
            let decision =
                store.ask_on_ticket(&ticket, &question, &options, recommend.as_deref(), &actor)?;
            if args.json {
                return print_json(&decision);
            }
            println!("Asked. {ticket} waits for the user's answer; other tickets keep running.");
            return Ok(());
        }
        TicketCommand::Report {
            ticket,
            outcome,
            conclusion,
            result_file,
        } => {
            let result = result_file.as_deref().map(read_json_file).transpose()?;
            let outcome = match outcome {
                OutcomeArg::Supports => Outcome::Supports,
                OutcomeArg::Refutes => Outcome::Refutes,
                OutcomeArg::Inconclusive => Outcome::Inconclusive,
            };
            store.report_ticket(&ticket, outcome, &conclusion, result.as_ref(), &actor)?
        }
        TicketCommand::Block { ticket, reason } => store.move_ticket(
            &ticket,
            TicketAction::Block,
            &actor,
            json!({"reason": reason}),
        )?,
        TicketCommand::Comment { ticket, text } => {
            store.comment_on_ticket(&ticket, &text, &actor)?;
            store.require_ticket(&ticket)?
        }
        TicketCommand::Taint { ticket, reason } => store.taint_ticket(&ticket, &reason, &actor)?,
    };
    if args.json {
        return print_json(&ticket);
    }
    println!("{} {} [{}]", ticket.key, ticket.title, ticket.status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_minutes;

    #[test]
    fn durations_read_as_minutes() {
        assert_eq!(parse_minutes("90m").unwrap(), 90);
        assert_eq!(parse_minutes("2h").unwrap(), 120);
        assert_eq!(parse_minutes("1h30m").unwrap(), 90);
        assert_eq!(parse_minutes("45").unwrap(), 45);
        for bad in ["soon", "0m", "1d", "h30"] {
            assert!(parse_minutes(bad).is_err(), "{bad}");
        }
    }
}
