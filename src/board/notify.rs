//! Phone notifications through ntfy. A push only says how many decisions wait:
//! nothing about the research leaves the machine before a patent is filed.

use serde::{Deserialize, Serialize};

/// Pushes at most this often; cards in between are counted in the next one.
pub const MIN_GAP_MS: i64 = 30_000;

/// `orx board set ntfy '{"url": "http://100.x.y.z:8080/orx"}'`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ntfy {
    /// The topic URL on the user's own ntfy server.
    pub url: String,
    /// An ntfy access token, when the server requires one.
    #[serde(default)]
    pub token: Option<String>,
    /// Opened when the notification is tapped, e.g. the dashboard's address.
    #[serde(default)]
    pub click_url: Option<String>,
}

/// What has been pushed so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pushed {
    /// The newest card creation time a push has covered.
    pub through: i64,
    pub at: i64,
}

/// Whether a push is due: a card newer than the last push waits, and the last
/// push is at least `MIN_GAP_MS` old.
pub fn due(open: usize, newest: Option<i64>, pushed: Pushed, now: i64) -> bool {
    open > 0
        && newest.is_some_and(|newest| newest > pushed.through)
        && now - pushed.at >= MIN_GAP_MS
}

/// The whole notification text.
pub fn message(open: usize) -> String {
    format!("ORX: {open} waiting for you")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_card_pushes_once_and_bursts_merge() {
        let none = Pushed::default();
        assert!(due(1, Some(100_000), none, 100_000));
        let pushed = Pushed {
            through: 100_000,
            at: 100_000,
        };
        assert!(!due(1, Some(100_000), pushed, 200_000), "nothing new");
        assert!(
            !due(2, Some(110_000), pushed, 110_000),
            "too soon: it waits"
        );
        assert!(
            due(3, Some(120_000), pushed, 130_000),
            "then one push covers both"
        );
        assert!(
            !due(0, Some(120_000), none, 130_000),
            "nothing waits any more"
        );
    }

    #[test]
    fn the_message_carries_only_a_count() {
        assert_eq!(message(3), "ORX: 3 waiting for you");
    }
}
