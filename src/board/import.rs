//! Drafting epics for a project's existing experiment tree. Pure: the store
//! reads the nodes, puts the draft on an import card, and writes epics and
//! tickets only once the user confirms it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// An experiment node as the import sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub created_at: i64,
}

/// One proposed epic and the nodes it holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub title: String,
    pub experiment_ids: Vec<String>,
}

pub const UNSORTED: &str = "Unsorted";

/// Leading words that open a question or a phrase rather than name a topic.
const NOT_TOPICS: &[&str] = &[
    "a", "an", "the", "does", "do", "is", "are", "can", "what", "which", "why", "how", "when",
    "where", "true", "full", "new", "more", "test", "eval", "check",
];

/// A word-led group needs this many nodes; a label like `E3` stands alone.
const MIN_WORD_GROUP: usize = 3;

/// Groups nodes by a leading label (`E1a` and `E1c` go to `E1`), then by a
/// shared leading word used by enough nodes; the rest are unsorted. Groups come
/// in the order their first node was created, unsorted last.
pub fn draft(nodes: &[Node]) -> Vec<Group> {
    let mut ordered: Vec<&Node> = nodes.iter().collect();
    ordered.sort_by_key(|node| node.created_at);
    let keys: Vec<(String, bool)> = ordered.iter().map(|node| key(&node.title)).collect();
    let mut word_counts: HashMap<&str, usize> = HashMap::new();
    for (key, labelled) in &keys {
        if !labelled {
            *word_counts.entry(key.as_str()).or_default() += 1;
        }
    }
    let mut groups: Vec<(String, Group)> = Vec::new();
    let mut unsorted = Vec::new();
    for (node, (key, labelled)) in ordered.iter().zip(&keys) {
        if !labelled
            && (word_counts[key.as_str()] < MIN_WORD_GROUP || NOT_TOPICS.contains(&key.as_str()))
        {
            unsorted.push(node.id.clone());
            continue;
        }
        match groups.iter_mut().find(|(seen, _)| seen == key) {
            Some((_, group)) => group.experiment_ids.push(node.id.clone()),
            None => groups.push((
                key.clone(),
                Group {
                    title: group_title(key, *labelled, &node.title),
                    experiment_ids: vec![node.id.clone()],
                },
            )),
        }
    }
    let mut groups: Vec<Group> = groups.into_iter().map(|(_, group)| group).collect();
    if !unsorted.is_empty() {
        groups.push(Group {
            title: UNSORTED.into(),
            experiment_ids: unsorted,
        });
    }
    groups
}

/// The grouping key of a title, and whether it is a label like `E1`.
fn key(title: &str) -> (String, bool) {
    if title.to_lowercase().contains("baseline reproduction") {
        return ("Baseline reproduction".into(), true);
    }
    if let Some(label) = label(title) {
        return (label, true);
    }
    let word = title
        .split(|c: char| c.is_whitespace() || c == ':' || c == '(')
        .next()
        .unwrap_or("")
        .trim_end_matches([',', '.'])
        .to_lowercase();
    (word, false)
}

/// `E1a`, `E2e-b`, `RP1`, `P6-fast` → `E1`, `E2`, `RP1`, `P6`: up to four
/// letters, a number, an optional variant letter (and `-x` sub-variant), then
/// the end of the word.
fn label(title: &str) -> Option<String> {
    let chars: Vec<char> = title.chars().collect();
    let mut i = 0;
    if !chars.first()?.is_ascii_uppercase() {
        return None;
    }
    while i < chars.len() && chars[i].is_ascii_alphabetic() && i < 4 {
        i += 1;
    }
    let digits = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits {
        return None;
    }
    let label: String = chars[..i].iter().collect();
    if i < chars.len() && chars[i].is_ascii_lowercase() {
        i += 1;
        if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1].is_ascii_lowercase() {
            i += 2;
        }
    }
    match chars.get(i) {
        None => Some(label),
        Some(c) if !c.is_alphanumeric() => Some(label),
        _ => None,
    }
}

fn group_title(key: &str, labelled: bool, first_title: &str) -> String {
    if !labelled || key == "Baseline reproduction" {
        return if labelled {
            key.to_string()
        } else {
            let word = first_title
                .split(|c: char| c.is_whitespace() || c == ':' || c == '(')
                .next()
                .unwrap_or(key);
            word.trim_end_matches([',', '.']).to_string()
        };
    }
    // `E1a robot role head: centroid grounding` → `E1: robot role head`.
    let rest = first_title
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest)
        .unwrap_or("");
    let rest = rest.split(':').next().unwrap_or("").trim();
    let rest: String = rest.chars().take(50).collect();
    if rest.is_empty() {
        key.to_string()
    } else {
        format!("{key}: {rest}")
    }
}

/// Checks a grouping against the nodes on the card: every group named, every
/// node in exactly one group. Empty groups are dropped.
pub fn validate(groups: &[Group], nodes: &[String]) -> Result<Vec<Group>, String> {
    let mut seen: HashMap<&str, &str> = HashMap::new();
    let mut kept = Vec::new();
    for group in groups {
        if group.experiment_ids.is_empty() {
            continue;
        }
        if group.title.trim().is_empty() {
            return Err("Every group needs a title.".into());
        }
        for id in &group.experiment_ids {
            if !nodes.contains(id) {
                return Err(format!("Experiment {id} is not on this import card."));
            }
            if let Some(other) = seen.insert(id, &group.title) {
                return Err(format!(
                    "Experiment {id} is in both `{other}` and `{}`.",
                    group.title
                ));
            }
        }
        kept.push(Group {
            title: group.title.trim().to_string(),
            experiment_ids: group.experiment_ids.clone(),
        });
    }
    if let Some(missing) = nodes.iter().find(|id| !seen.contains_key(id.as_str())) {
        return Err(format!("Experiment {missing} is in no group."));
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(titles: &[&str]) -> Vec<Node> {
        titles
            .iter()
            .enumerate()
            .map(|(i, title)| Node {
                id: format!("n{i}"),
                title: title.to_string(),
                created_at: i as i64,
            })
            .collect()
    }

    fn titles(groups: &[Group]) -> Vec<(&str, usize)> {
        groups
            .iter()
            .map(|group| (group.title.as_str(), group.experiment_ids.len()))
            .collect()
    }

    #[test]
    fn slot_jepa_style_labels_group_by_their_number() {
        let groups = draft(&nodes(&[
            "PushT Baseline Reproduction",
            "Maze Baseline Reproduction",
            "Slot swap diagnostic",
            "E1a robot role head: centroid grounding",
            "E1c dynamic-slot calibration: peak",
            "E2e-a object role: per-patch head + Otsu",
            "E2e-b object role: per-patch head + self-training",
            "E9 TwoRoomR planning: robot-slot-only cost",
        ]));
        assert_eq!(
            titles(&groups),
            [
                ("Baseline reproduction", 2),
                ("E1: robot role head", 2),
                ("E2: object role", 2),
                ("E9: TwoRoomR planning", 1),
                (UNSORTED, 1),
            ]
        );
    }

    #[test]
    fn free_titles_group_by_a_shared_leading_word() {
        let groups = draft(&nodes(&[
            "Joint-SAGE P1: closed-loop subgoal replanning",
            "Joint-SAGE third training seed (0)",
            "Joint-SAGE-W3: stronger actor adaptation",
            "joint-sage: lowercase variant",
            "Probe: action-sensitive footprint",
            "Probe v2: per-clip outputs",
            "LeWM Flat Baseline",
            "Does the mask rotate?",
            "Does far-goal supervision help?",
            "Does a tighter budget help?",
        ]));
        assert_eq!(titles(&groups), [("Joint-SAGE", 3), (UNSORTED, 7)]);
    }

    #[test]
    fn labels_need_a_number_and_a_word_boundary() {
        assert_eq!(label("E2e-b object"), Some("E2".into()));
        assert_eq!(label("RP1 IX-FrRP"), Some("RP1".into()));
        assert_eq!(label("P6-fast: recipe"), Some("P6".into()));
        assert_eq!(label("E2E-Chord-A2: end to end"), None);
        assert_eq!(label("Table 3 ablation"), None);
        assert_eq!(label("Stage-1 eval"), None);
        assert_eq!(label("e1 lowercase"), None);
    }

    #[test]
    fn a_confirmed_grouping_places_every_node_once() {
        let nodes = vec!["a".to_string(), "b".to_string()];
        let group = |title: &str, ids: &[&str]| Group {
            title: title.into(),
            experiment_ids: ids.iter().map(|id| id.to_string()).collect(),
        };
        assert_eq!(
            validate(&[group("One", &["a", "b"]), group("Empty", &[])], &nodes)
                .unwrap()
                .len(),
            1
        );
        assert!(validate(&[group("One", &["a"])], &nodes)
            .unwrap_err()
            .contains("in no group"));
        assert!(
            validate(&[group("One", &["a", "b"]), group("Two", &["b"])], &nodes)
                .unwrap_err()
                .contains("in both")
        );
        assert!(validate(&[group(" ", &["a", "b"])], &nodes).is_err());
        assert!(validate(&[group("One", &["a", "b", "z"])], &nodes).is_err());
    }
}
