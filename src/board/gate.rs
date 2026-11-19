//! The board's gates: which experiments a chat may create, which machines a
//! run may use, and when a confidential project may be published. Pure: the
//! store and the server read the inputs and act on the verdict.

use super::model::{Machine, ProposedBy, Ticket, TicketKind, TicketStatus};

/// What creating an experiment in a board project comes to.
#[derive(Clone, Debug, PartialEq)]
pub enum CreateVerdict {
    /// The chat works an open ticket the experiment belongs to.
    Allow,
    /// The user asked for it in their own chat: it gets an unplanned ticket, approved by asking.
    Unplanned,
    Deny(String),
}

/// `bound` is the ticket the creating chat last worked; `human_turn` says the
/// user, not orx or another chat, started the turn the chat is in.
pub fn create_verdict(bound: Option<&Ticket>, human_turn: bool) -> CreateVerdict {
    if let Some(ticket) = bound {
        let open = matches!(ticket.status, TicketStatus::Running | TicketStatus::Waiting);
        if open && ticket.kind == TicketKind::Experiment {
            match ticket.proposed_by {
                // A user-chat ticket covers the turn the user asked in, not the agent's later turns.
                ProposedBy::UserChat if ticket.experiment_id.is_none() && human_turn => {
                    return CreateVerdict::Allow
                }
                ProposedBy::UserChat => {}
                _ => return CreateVerdict::Allow,
            }
        }
    }
    if human_turn {
        return CreateVerdict::Unplanned;
    }
    let why = match bound {
        Some(ticket) if ticket.kind == TicketKind::Analysis => format!(
            "this chat works {}, an analysis ticket, which runs no experiment",
            ticket.key
        ),
        Some(ticket) if ticket.proposed_by != ProposedBy::UserChat => {
            format!("this chat's ticket {} is {}", ticket.key, ticket.status)
        }
        _ => "the user did not ask for it in this turn".to_string(),
    };
    CreateVerdict::Deny(format!(
        "This project plans its experiments on the research board, and {why}. \
         Propose it with `orx ticket propose --epic <epic> --title \"…\" --hypothesis \"…\" \
         --est 2h --validation-file '{{\"metrics\":[…],\"every\":…,\"logs\":…}}'` \
         and wait for the user to approve it; a chat the board starts for it will create the experiment."
    ))
}

/// The backend a launch names, in the form machines are registered under.
pub fn launch_target(backend: &str, host: Option<&str>) -> String {
    match (backend, host) {
        ("ssh", Some(host)) => format!("ssh:{host}"),
        (backend, _) => backend.to_string(),
    }
}

/// Whether a run may launch on `target`. A ticket's run goes to the machine its
/// GPUs were reserved on; a confidential project runs only on this machine or
/// on a registered machine the user owns.
pub fn backend_verdict(
    confidential: bool,
    assigned: Option<(&Ticket, &Machine)>,
    machines: &[Machine],
    target: &str,
) -> Result<(), String> {
    if let Some((ticket, machine)) = assigned {
        if machine.backend != target {
            return Err(format!(
                "{} holds its GPUs on {} ({}), so its runs launch there: {}. \
                 If it has to run elsewhere (where its data is), pin it there first: \
                 `orx ticket pin {} --machine <name>`.",
                ticket.key,
                machine.name,
                machine.backend,
                launch_flags(&machine.backend),
                ticket.key
            ));
        }
        return Ok(());
    }
    if !confidential || target == "local" {
        return Ok(());
    }
    let own = machines
        .iter()
        .any(|machine| machine.own && machine.enabled && machine.backend == target);
    if own {
        return Ok(());
    }
    let mut choices = vec!["local".to_string()];
    for machine in machines
        .iter()
        .filter(|machine| machine.own && machine.enabled)
    {
        if !choices.contains(&machine.backend) {
            choices.push(machine.backend.clone());
        }
    }
    Err(format!(
        "This project is confidential until its patent is filed, so its runs stay on machines the user owns, \
         and `{target}` is not one of them. Use {}. The user can register an owned SSH machine with \
         `orx board machines add`.",
        choices
            .iter()
            .map(|backend| launch_flags(backend))
            .collect::<Vec<_>>()
            .join(" or ")
    ))
}

fn launch_flags(backend: &str) -> String {
    match backend.strip_prefix("ssh:") {
        Some(host) => format!("`--backend ssh --host {host}`"),
        None => format!("`--backend {backend}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(status: TicketStatus, by: ProposedBy, experiment: Option<&str>) -> Ticket {
        Ticket {
            id: "t".into(),
            project_id: "p".into(),
            epic_id: None,
            key: "SJ-001".into(),
            kind: TicketKind::Experiment,
            batch_id: None,
            title: "lr".into(),
            hypothesis: None,
            change_spec: None,
            status,
            priority: 100,
            base_experiment_id: None,
            experiment_id: experiment.map(Into::into),
            needs_gpus: 1,
            est_minutes: None,
            validation: None,
            harness: None,
            session_id: Some("chat".into()),
            machine_id: None,
            pinned_machine_id: None,
            repair_count: 0,
            outcome: None,
            core_hypothesis: false,
            result: None,
            conclusion: None,
            tainted: false,
            tainted_reason: None,
            proposed_by: by,
            started_at: None,
            finished_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn machine(name: &str, backend: &str, own: bool) -> Machine {
        Machine {
            id: name.into(),
            name: name.into(),
            backend: backend.into(),
            gpu_slots: 2,
            own,
            enabled: true,
            updated_at: 0,
        }
    }

    #[test]
    fn an_executor_creates_experiments_for_its_running_ticket() {
        let running = ticket(TicketStatus::Running, ProposedBy::Agent, Some("exp"));
        assert_eq!(create_verdict(Some(&running), false), CreateVerdict::Allow);
        let done = ticket(TicketStatus::Done, ProposedBy::Agent, Some("exp"));
        let CreateVerdict::Deny(why) = create_verdict(Some(&done), false) else {
            panic!("a finished ticket cannot grow experiments");
        };
        assert!(
            why.contains("SJ-001 is done") && why.contains("orx ticket propose"),
            "{why}"
        );
    }

    #[test]
    fn the_user_asking_in_their_own_chat_opens_an_unplanned_ticket() {
        assert_eq!(create_verdict(None, true), CreateVerdict::Unplanned);
        let used = ticket(TicketStatus::Running, ProposedBy::UserChat, Some("exp"));
        assert_eq!(create_verdict(Some(&used), true), CreateVerdict::Unplanned);
        let fresh = ticket(TicketStatus::Running, ProposedBy::UserChat, None);
        assert_eq!(create_verdict(Some(&fresh), true), CreateVerdict::Allow);
    }

    #[test]
    fn an_agent_acting_on_its_own_is_refused() {
        assert!(matches!(
            create_verdict(None, false),
            CreateVerdict::Deny(_)
        ));
        let asked = ticket(TicketStatus::Running, ProposedBy::UserChat, Some("exp"));
        let CreateVerdict::Deny(why) = create_verdict(Some(&asked), false) else {
            panic!("a later agent turn needs its own ticket");
        };
        assert!(why.contains("did not ask for it"), "{why}");
    }

    #[test]
    fn a_ticket_run_launches_on_its_reserved_machine() {
        let running = ticket(TicketStatus::Running, ProposedBy::Agent, None);
        let gpu = machine("gpu1", "ssh:gpu1", true);
        assert!(backend_verdict(false, Some((&running, &gpu)), &[], "ssh:gpu1").is_ok());
        let why = backend_verdict(false, Some((&running, &gpu)), &[], "local").unwrap_err();
        assert!(why.contains("--backend ssh --host gpu1"), "{why}");
    }

    #[test]
    fn confidential_runs_stay_on_owned_machines() {
        let machines = [
            machine("rented", "ssh:rented", false),
            machine("lab", "ssh:lab", true),
            machine("ws", "local", true),
        ];
        assert!(backend_verdict(true, None, &machines, "local").is_ok());
        assert!(backend_verdict(true, None, &machines, "ssh:lab").is_ok());
        for target in ["ssh:rented", "modal", "hf", "openresearch"] {
            let why = backend_verdict(true, None, &machines, target).unwrap_err();
            assert!(
                why.contains("confidential") && why.contains("--backend ssh --host lab"),
                "{why}"
            );
            assert_eq!(why.matches("--backend local").count(), 1, "{why}");
        }
        assert!(backend_verdict(false, None, &machines, "modal").is_ok());
        assert_eq!(launch_target("ssh", Some("lab")), "ssh:lab");
        assert_eq!(launch_target("modal", None), "modal");
    }
}
