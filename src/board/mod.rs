//! The research board: epics, tickets and the user's decisions that plan and
//! gate experiment work. Records live in the store (`store::board`); this
//! module owns their vocabulary and the rules for moving them.

pub mod model;
pub mod transitions;

pub use model::*;
pub use transitions::TicketAction;
