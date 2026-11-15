//! Which ticket moves are legal, and who may make each one.
//!
//! The user decides (approve, reject, accept, answer, unblock); agents report
//! and ask; the board dispatches and blocks. Epic rules may let the board make
//! a user's move, which callers express as `Actor::System(Some(rule))`.

use super::model::{Actor, TicketStatus};
use crate::error::{bail, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicketAction {
    Approve,
    Reject,
    /// The scheduler starts the ticket on a machine.
    Dispatch,
    /// The executor asks the user something; only this ticket waits.
    Ask,
    Answer,
    /// The executor hands its result in for review.
    Report,
    Accept,
    /// The user disputes a reported result; the executor picks it back up.
    Challenge,
    Block,
    /// The user clears a block, sending the ticket back to `ready`.
    Unblock,
    /// The scheduler hands a dispatched ticket back, as when its start failed.
    Requeue,
    Cancel,
}

impl TicketAction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Dispatch => "dispatch",
            Self::Ask => "ask",
            Self::Answer => "answer",
            Self::Report => "report",
            Self::Accept => "accept",
            Self::Challenge => "challenge",
            Self::Block => "block",
            Self::Unblock => "unblock",
            Self::Requeue => "requeue",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Who {
    User,
    Agent,
    Board,
}

fn who(actor: &Actor) -> Who {
    match actor {
        Actor::User => Who::User,
        Actor::Agent(_) => Who::Agent,
        Actor::System(_) => Who::Board,
    }
}

/// The status `action` moves a ticket in `from` to, or why `actor` may not.
pub fn next(from: TicketStatus, action: TicketAction, actor: &Actor) -> Result<TicketStatus> {
    use TicketAction as A;
    use TicketStatus as S;
    let by = who(actor);
    // A rule may stand in for the user only where the user would approve or accept.
    let user_or_rule = |rule_allowed: bool| {
        by == Who::User || (rule_allowed && matches!(actor, Actor::System(Some(_))))
    };
    let allowed = match action {
        A::Approve | A::Accept => user_or_rule(true),
        A::Reject | A::Answer | A::Challenge | A::Unblock | A::Cancel => by == Who::User,
        A::Dispatch | A::Requeue => by == Who::Board,
        A::Ask | A::Report => by == Who::Agent,
        A::Block => by != Who::User,
    };
    if !allowed {
        bail!(
            "{} cannot {} a ticket: {}",
            match by {
                Who::User => "You",
                Who::Agent => "An agent",
                Who::Board => "The board",
            },
            action.name(),
            match by {
                Who::Agent => "only the user approves, rejects, accepts or answers. Use `orx ticket propose`, `orx ticket ask` or `orx ticket report` instead.",
                _ => "this step belongs to the agent working the ticket or to the scheduler.",
            }
        );
    }
    let to = match (from, action) {
        (S::Proposed, A::Approve) => S::Ready,
        (S::Proposed, A::Reject) => S::Rejected,
        (S::Ready, A::Dispatch) => S::Running,
        (S::Running, A::Requeue) => S::Ready,
        (S::Running, A::Ask) => S::Waiting,
        (S::Waiting, A::Answer) => S::Running,
        (S::Running, A::Report) => S::Review,
        (S::Review, A::Accept) => S::Done,
        (S::Review, A::Challenge) => S::Running,
        (S::Running | S::Waiting, A::Block) => S::Blocked,
        (S::Blocked, A::Unblock) => S::Ready,
        (S::Blocked, A::Reject) => S::Rejected,
        (S::Proposed | S::Ready | S::Running | S::Waiting | S::Review | S::Blocked, A::Cancel) => {
            S::Cancelled
        }
        _ => bail!(
            "Cannot {} this ticket: it is {}, and it has to be {} first.",
            action.name(),
            from,
            expected_from(action)
        ),
    };
    Ok(to)
}

fn expected_from(action: TicketAction) -> &'static str {
    use TicketAction as A;
    match action {
        A::Approve => "proposed",
        A::Reject => "proposed or blocked",
        A::Dispatch => "ready",
        A::Ask | A::Report | A::Requeue => "running",
        A::Answer => "waiting",
        A::Accept | A::Challenge => "in review",
        A::Block => "running or waiting",
        A::Unblock => "blocked",
        A::Cancel => "still open",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TicketAction as A;
    use TicketStatus as S;

    fn agent() -> Actor {
        Actor::Agent("chat-1".into())
    }

    fn board() -> Actor {
        Actor::System(None)
    }

    #[test]
    fn a_ticket_walks_from_proposal_to_done_with_each_step_by_its_owner() {
        let mut status = S::Proposed;
        for (action, actor, expected) in [
            (A::Approve, Actor::User, S::Ready),
            (A::Dispatch, board(), S::Running),
            (A::Ask, agent(), S::Waiting),
            (A::Answer, Actor::User, S::Running),
            (A::Report, agent(), S::Review),
            (A::Challenge, Actor::User, S::Running),
            (A::Report, agent(), S::Review),
            (A::Accept, Actor::User, S::Done),
        ] {
            status = next(status, action, &actor).unwrap();
            assert_eq!(status, expected, "after {action:?}");
        }
    }

    #[test]
    fn agents_cannot_make_the_users_decisions() {
        for (from, action) in [
            (S::Proposed, A::Approve),
            (S::Proposed, A::Reject),
            (S::Review, A::Accept),
            (S::Waiting, A::Answer),
            (S::Blocked, A::Unblock),
            (S::Running, A::Cancel),
        ] {
            let error = next(from, action, &agent()).unwrap_err().to_string();
            assert!(error.contains("only the user"), "{action:?}: {error}");
        }
    }

    #[test]
    fn only_a_named_rule_may_approve_or_accept_for_the_user() {
        let rule = Actor::System(Some("auto_done".into()));
        assert_eq!(next(S::Review, A::Accept, &rule).unwrap(), S::Done);
        assert_eq!(next(S::Proposed, A::Approve, &rule).unwrap(), S::Ready);
        assert!(next(S::Review, A::Accept, &board()).is_err());
        assert!(next(S::Proposed, A::Reject, &rule).is_err());
    }

    #[test]
    fn the_scheduler_alone_dispatches_and_the_user_cannot_block() {
        assert!(next(S::Ready, A::Dispatch, &Actor::User).is_err());
        assert!(next(S::Ready, A::Dispatch, &agent()).is_err());
        assert!(next(S::Running, A::Block, &Actor::User).is_err());
        assert_eq!(next(S::Running, A::Block, &agent()).unwrap(), S::Blocked);
        assert_eq!(next(S::Running, A::Requeue, &board()).unwrap(), S::Ready);
    }

    #[test]
    fn moves_from_the_wrong_column_say_where_the_ticket_must_be() {
        let error = next(S::Ready, A::Accept, &Actor::User)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ready") && error.contains("in review"),
            "{error}"
        );
        assert!(next(S::Done, A::Cancel, &Actor::User).is_err());
        assert!(next(S::Rejected, A::Approve, &Actor::User).is_err());
    }
}
