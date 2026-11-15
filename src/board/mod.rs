//! The research board: epics, tickets and the user's decisions that plan and
//! gate experiment work. Records live in the store (`store::board`); this
//! module owns their vocabulary and the rules for moving them.

pub mod brief;
pub mod checks;
pub mod gate;
pub mod model;
pub mod scheduler;
pub mod transitions;

pub use model::*;
pub use transitions::TicketAction;

use crate::store::Store;

/// The `<orx-ticket>` block for a chat working an open ticket, if it works one.
pub fn session_context(store: &Store, session_id: &str) -> Option<String> {
    let ticket = match store.ticket_for_session(session_id) {
        Ok(Some(ticket)) => ticket,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("orx up: could not read the board ticket for chat {session_id}: {error}");
            return None;
        }
    };
    if !matches!(
        ticket.status,
        TicketStatus::Running | TicketStatus::Waiting | TicketStatus::Review
    ) {
        return None;
    }
    let epic = ticket
        .epic_id
        .as_deref()
        .and_then(|id| store.epic(id).ok().flatten());
    let machine = ticket
        .machine_id
        .as_deref()
        .and_then(|id| store.machine(id).ok().flatten());
    Some(brief::turn_context(
        &ticket,
        epic.as_ref(),
        machine.as_ref(),
    ))
}
