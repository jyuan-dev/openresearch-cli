//! Which ready tickets start next, and on which machine. Pure: the caller
//! reads the store into these inputs and applies the plan.

use std::collections::HashMap;

/// A ready ticket whose epic is active, with what dispatch has to weigh.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub ticket_id: String,
    pub project_priority: i64,
    pub epic_priority: i64,
    pub priority: i64,
    pub created_at: i64,
    pub needs_gpus: i64,
    pub confidential: bool,
    /// A ticket it is `blocked_by` has not reached done.
    pub waiting_on_dependency: bool,
    /// Its epic has spent its GPU hours or passed its deadline.
    pub out_of_budget: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MachineSlots {
    pub machine_id: String,
    pub gpu_slots: i64,
    pub in_use: i64,
    pub own: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Start {
    pub ticket_id: String,
    /// The machine holding its GPUs; none for a ticket that needs no GPU.
    pub machine_id: Option<String>,
}

/// Tickets to start now, in priority order: lower project, epic and ticket
/// priority first, then the oldest. A ticket that does not fit waits without
/// holding back smaller ones behind it.
pub fn plan(
    candidates: &[Candidate],
    machines: &[MachineSlots],
    live_sessions: usize,
    max_sessions: usize,
) -> Vec<Start> {
    let mut ordered: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| !candidate.waiting_on_dependency && !candidate.out_of_budget)
        .collect();
    ordered.sort_by(|a, b| {
        (
            a.project_priority,
            a.epic_priority,
            a.priority,
            a.created_at,
        )
            .cmp(&(
                b.project_priority,
                b.epic_priority,
                b.priority,
                b.created_at,
            ))
    });
    let mut free: HashMap<&str, i64> = machines
        .iter()
        .map(|machine| {
            (
                machine.machine_id.as_str(),
                machine.gpu_slots - machine.in_use,
            )
        })
        .collect();
    let mut sessions = live_sessions;
    let mut starts = Vec::new();
    for candidate in ordered {
        if sessions >= max_sessions {
            break;
        }
        let machine_id = if candidate.needs_gpus == 0 {
            None
        } else {
            // The machine with the fewest free slots that still fits keeps big gaps for big tickets.
            let fit = machines
                .iter()
                .filter(|machine| machine.own || !candidate.confidential)
                .filter(|machine| free[machine.machine_id.as_str()] >= candidate.needs_gpus)
                .min_by_key(|machine| {
                    (
                        free[machine.machine_id.as_str()],
                        machine.machine_id.as_str(),
                    )
                });
            match fit {
                Some(machine) => Some(machine.machine_id.clone()),
                None => continue,
            }
        };
        if let Some(machine_id) = &machine_id {
            *free
                .get_mut(machine_id.as_str())
                .expect("planned on a listed machine") -= candidate.needs_gpus;
        }
        sessions += 1;
        starts.push(Start {
            ticket_id: candidate.ticket_id.clone(),
            machine_id,
        });
    }
    starts
}

/// GPU hours spent, from `(started_ms, ended_ms or now, gpus)` per run.
pub fn gpu_hours(runs: &[(i64, i64, i64)]) -> f64 {
    runs.iter()
        .map(|(started, ended, gpus)| {
            ((ended - started).max(0) as f64 / 3_600_000.0) * *gpus as f64
        })
        .sum()
}

/// The budget thresholds `spent` has crossed: 80 and 100 percent of `budget`.
pub fn budget_marks(spent: f64, budget: Option<f64>) -> Vec<u8> {
    match budget {
        Some(budget) if budget > 0.0 => [80u8, 100]
            .into_iter()
            .filter(|mark| spent >= budget * f64::from(*mark) / 100.0)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, project: i64, gpus: i64, created: i64) -> Candidate {
        Candidate {
            ticket_id: id.into(),
            project_priority: project,
            epic_priority: 100,
            priority: 100,
            created_at: created,
            needs_gpus: gpus,
            confidential: true,
            waiting_on_dependency: false,
            out_of_budget: false,
        }
    }

    fn machine(id: &str, slots: i64, in_use: i64, own: bool) -> MachineSlots {
        MachineSlots {
            machine_id: id.into(),
            gpu_slots: slots,
            in_use,
            own,
        }
    }

    fn ids(starts: &[Start]) -> Vec<&str> {
        starts
            .iter()
            .map(|start| start.ticket_id.as_str())
            .collect()
    }

    #[test]
    fn higher_priority_projects_go_first_and_gpus_are_not_overbooked() {
        let starts = plan(
            &[
                candidate("late", 2, 1, 1),
                candidate("first", 1, 1, 5),
                candidate("second", 1, 1, 6),
            ],
            &[machine("ws", 2, 0, true)],
            0,
            8,
        );
        assert_eq!(ids(&starts), ["first", "second"]);
        assert!(starts
            .iter()
            .all(|start| start.machine_id.as_deref() == Some("ws")));
    }

    #[test]
    fn a_ticket_that_does_not_fit_lets_smaller_ones_through() {
        let starts = plan(
            &[
                candidate("big", 1, 4, 1),
                candidate("small", 1, 1, 2),
                candidate("cpu", 1, 0, 3),
            ],
            &[machine("ws", 2, 1, true)],
            0,
            8,
        );
        assert_eq!(ids(&starts), ["small", "cpu"]);
        assert_eq!(starts[1].machine_id, None);
    }

    #[test]
    fn confidential_tickets_only_use_own_machines() {
        let mut open = candidate("open", 1, 1, 2);
        open.confidential = false;
        let starts = plan(
            &[candidate("secret", 1, 1, 1), open],
            &[machine("rented", 4, 0, false)],
            0,
            8,
        );
        assert_eq!(ids(&starts), ["open"]);
    }

    #[test]
    fn dependencies_budgets_and_the_session_cap_hold_tickets_back() {
        let mut blocked = candidate("blocked", 1, 0, 1);
        blocked.waiting_on_dependency = true;
        let mut broke = candidate("broke", 1, 0, 2);
        broke.out_of_budget = true;
        let starts = plan(
            &[
                blocked,
                broke,
                candidate("a", 1, 0, 3),
                candidate("b", 1, 0, 4),
            ],
            &[],
            7,
            8,
        );
        assert_eq!(ids(&starts), ["a"]);
    }

    #[test]
    fn the_tightest_fitting_machine_is_used() {
        let starts = plan(
            &[candidate("one", 1, 1, 1)],
            &[machine("big", 8, 0, true), machine("small", 2, 1, true)],
            0,
            8,
        );
        assert_eq!(starts[0].machine_id.as_deref(), Some("small"));
    }

    #[test]
    fn gpu_hours_and_budget_marks() {
        let spent = gpu_hours(&[(0, 3_600_000, 2), (0, 1_800_000, 1), (10, 5, 1)]);
        assert!((spent - 2.5).abs() < 1e-9);
        assert_eq!(budget_marks(8.0, Some(10.0)), [80]);
        assert_eq!(budget_marks(10.0, Some(10.0)), [80, 100]);
        assert!(budget_marks(100.0, None).is_empty());
    }
}
