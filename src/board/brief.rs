//! What the board tells the agent working a ticket: the task it starts with,
//! and the `<orx-ticket>` block every later turn carries.

use super::model::{Criteria, Epic, Machine, Ticket, TicketKind};

fn line(label: &str, value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("{label}: {value}\n"))
        .unwrap_or_default()
}

fn backend_flag(machine: Option<&Machine>) -> String {
    match machine {
        Some(machine) => format!("`--backend {}`", machine.backend),
        None => "no GPU backend (this ticket holds no GPU)".into(),
    }
}

/// The first message of a ticket's chat.
pub fn executor_task(
    ticket: &Ticket,
    epic: Option<&Epic>,
    criteria: Option<&Criteria>,
    machine: Option<&Machine>,
) -> String {
    let mut task = format!(
        "[orx board] You are working ticket {} — {}.\n\n",
        ticket.key, ticket.title
    );
    if let Some(epic) = epic {
        task.push_str(&format!(
            "It belongs to {} ({}): {}\n",
            epic.key, epic.title, epic.question
        ));
    }
    task.push_str(&line("Hypothesis", ticket.hypothesis.as_deref()));
    task.push_str(&line("Change to make", ticket.change_spec.as_deref()));
    if let Some(criteria) = criteria {
        task.push_str(&format!(
            "Answer criteria v{}:\n{}\n",
            criteria.version,
            criteria.body_md.trim()
        ));
    }
    task.push('\n');
    match ticket.kind {
        TicketKind::Experiment => {
            let parent = ticket
                .base_experiment_id
                .as_deref()
                .map(|base| format!("`--parent {base}`"))
                // Never the project's current root: that may be another ticket's experiment.
                .unwrap_or_else(|| {
                    "nothing: pass `--baseline` so it starts a root of its own".into()
                });
            let validation = ticket
                .validation
                .as_ref()
                .map(|validation| validation.to_string())
                .unwrap_or_else(|| "{}".into());
            task.push_str(&format!(
                "Steps:\n\
                 1. Create exactly one experiment for this ticket with `orx create-experiment` branching from {parent}; the board links it to {key}.\n\
                 2. Make only the change above on its branch, and log the validation the ticket declares: {validation}.\n\
                 3. Launch it with `orx exp run <expId>` and {backend}. Do not use another backend: the board reserved this machine's GPUs for you.\n\
                 4. When the run's result reaches this chat, read its logs and report with\n   \
                    `orx ticket report {key} --outcome supports|refutes|inconclusive --conclusion \"…\" --result-file result.json`,\n   \
                    where result.json holds `{{\"metric\": …, \"values\": [one value per seed], \"baseline\": …}}`.\n",
                key = ticket.key,
                backend = backend_flag(machine),
            ));
        }
        TicketKind::Analysis => task.push_str(&format!(
            "Do the analysis, save what you produce as project artifacts, then report with\n\
             `orx ticket report {} --outcome supports|refutes|inconclusive --conclusion \"…\"`.\n",
            ticket.key
        )),
    }
    task.push_str(&format!(
        "\nIf a run fails for a reason you can fix, fix it and run again on the same experiment. \
         If you need the user to decide something, ask with `orx ticket ask {key} --question \"…\" --option A --option B --recommend A` and stop; \
         only this ticket waits. If you cannot go on, `orx ticket block {key} --reason \"…\"`. \
         You cannot approve, accept or create approved tickets; to suggest more work, `orx ticket propose` it.",
        key = ticket.key
    ));
    task
}

/// What the board tells a ticket's chat about the user's answer, challenge or
/// comment, made in the dashboard or the CLI rather than in the chat.
pub fn user_message(ticket: &Ticket, kind: &str, detail: &serde_json::Value) -> Option<String> {
    let inner = &detail["detail"];
    let text = |key: &str| {
        inner[key]
            .as_str()
            .or_else(|| detail[key].as_str())
            .map(str::to_string)
    };
    match kind {
        "answer" => Some(format!(
            "[orx board] The user answered your question on {}: {}\nContinue the ticket with that answer.",
            ticket.key,
            text("answer")?
        )),
        "challenge" => Some(format!(
            "[orx board] The user disputes your result on {}: {}\nLook at it again, then report again with `orx ticket report {}`.",
            ticket.key,
            text("note").unwrap_or_else(|| "no reason given".into()),
            ticket.key
        )),
        "comment" => Some(format!(
            "[orx board] The user commented on {}: {}",
            ticket.key,
            text("text")?
        )),
        _ => None,
    }
}

/// The block every turn of a ticket's chat carries, so the ticket survives compaction.
pub fn turn_context(ticket: &Ticket, epic: Option<&Epic>, machine: Option<&Machine>) -> String {
    let mut context = format!(
        "<orx-ticket key=\"{}\" status=\"{}\"{}>\n{}\n",
        ticket.key,
        ticket.status,
        epic.map(|epic| format!(" epic=\"{}\"", epic.key))
            .unwrap_or_default(),
        ticket.title
    );
    context.push_str(&line("Hypothesis", ticket.hypothesis.as_deref()));
    context.push_str(&line("Change", ticket.change_spec.as_deref()));
    context.push_str(&line("Experiment", ticket.experiment_id.as_deref()));
    if ticket.kind == TicketKind::Experiment {
        context.push_str(&format!("Run on: {}\n", backend_flag(machine)));
    }
    context.push_str(&format!(
        "Report with `orx ticket report {key}`, ask with `orx ticket ask {key}`, block with `orx ticket block {key}`.\n</orx-ticket>",
        key = ticket.key
    ));
    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::model::{ProposedBy, TicketStatus};

    fn ticket(kind: TicketKind) -> Ticket {
        Ticket {
            id: "t1".into(),
            project_id: "p".into(),
            epic_id: None,
            key: "SJ-007".into(),
            kind,
            batch_id: None,
            title: "lr 3e-4".into(),
            hypothesis: Some("faster convergence".into()),
            change_spec: Some("set lr in config.yaml".into()),
            status: TicketStatus::Running,
            priority: 100,
            base_experiment_id: Some("exp-base".into()),
            experiment_id: None,
            needs_gpus: 1,
            est_minutes: Some(60),
            validation: Some(serde_json::json!({"metric": "val_loss"})),
            harness: None,
            session_id: None,
            machine_id: None,
            pinned_machine_id: None,
            repair_count: 0,
            outcome: None,
            core_hypothesis: false,
            result: None,
            conclusion: None,
            tainted: false,
            tainted_reason: None,
            proposed_by: ProposedBy::Agent,
            started_at: None,
            finished_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn machine() -> Machine {
        Machine {
            id: "m".into(),
            name: "ws".into(),
            backend: "ssh:gpu1".into(),
            gpu_slots: 2,
            own: true,
            enabled: true,
            updated_at: 0,
        }
    }

    #[test]
    fn an_experiment_task_names_its_parent_backend_and_how_to_report() {
        let task = executor_task(
            &ticket(TicketKind::Experiment),
            None,
            None,
            Some(&machine()),
        );
        assert!(task.contains("--parent exp-base"));
        let mut rootless = ticket(TicketKind::Experiment);
        rootless.base_experiment_id = None;
        assert!(executor_task(&rootless, None, None, None).contains("--baseline"));
        assert!(task.contains("--backend ssh:gpu1"));
        assert!(task.contains("orx ticket report SJ-007"));
        assert!(task.contains("\"values\""));
        assert!(task.contains("val_loss"));
    }

    #[test]
    fn an_analysis_task_runs_no_experiment() {
        let task = executor_task(&ticket(TicketKind::Analysis), None, None, None);
        assert!(!task.contains("create-experiment"));
        assert!(task.contains("orx ticket report SJ-007"));
    }

    #[test]
    fn the_users_answers_challenges_and_comments_read_as_board_messages() {
        let ticket = ticket(TicketKind::Experiment);
        let answer = user_message(
            &ticket,
            "answer",
            &serde_json::json!({"detail": {"answer": "use val"}}),
        )
        .unwrap();
        assert!(answer.contains("SJ-007") && answer.contains("use val"));
        let challenge = user_message(
            &ticket,
            "challenge",
            &serde_json::json!({"detail": {"note": "seed 4"}}),
        )
        .unwrap();
        assert!(challenge.contains("seed 4") && challenge.contains("orx ticket report SJ-007"));
        assert!(
            user_message(&ticket, "comment", &serde_json::json!({"text": "nice"}))
                .unwrap()
                .contains("nice")
        );
        assert!(user_message(&ticket, "approve", &serde_json::json!({})).is_none());
    }

    #[test]
    fn the_turn_context_carries_the_key_status_and_backend() {
        let context = turn_context(&ticket(TicketKind::Experiment), None, Some(&machine()));
        assert!(context.starts_with("<orx-ticket key=\"SJ-007\" status=\"running\">"));
        assert!(context.contains("--backend ssh:gpu1"));
        assert!(context.ends_with("</orx-ticket>"));
    }
}
