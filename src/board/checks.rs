//! Machine checks on a reported result, before the user is asked to accept it.
//!
//! The executor reports per-seed values of the epic's metric and the
//! baseline's value. A result that passes every check can be accepted in a
//! batch; one that fails goes on its own card with the reasons.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::Outcome;

/// Check parameters: global defaults, overridden per epic by its approved criteria.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Checks {
    /// The metric the result must report; any metric when unset.
    pub metric: Option<String>,
    pub higher_is_better: bool,
    pub min_seeds: usize,
    /// A difference within this distance of the baseline is noise.
    pub noise_floor: Option<f64>,
    /// The smallest improvement that counts as support.
    pub min_delta: Option<f64>,
    pub require_finite: bool,
}

impl Default for Checks {
    fn default() -> Self {
        Self {
            metric: None,
            higher_is_better: true,
            min_seeds: 3,
            noise_floor: None,
            min_delta: None,
            require_finite: true,
        }
    }
}

impl Checks {
    /// `defaults` with each field `overrides` names replaced.
    pub fn merged(defaults: &Value, overrides: &Value) -> Self {
        let mut merged = serde_json::to_value(Checks::default()).unwrap_or(Value::Null);
        for layer in [defaults, overrides] {
            if let (Some(target), Some(layer)) = (merged.as_object_mut(), layer.as_object()) {
                for (key, value) in layer {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        serde_json::from_value(merged).unwrap_or_default()
    }
}

/// What the executor reports in `--result-file`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReportedResult {
    pub metric: Option<String>,
    /// One value per seed. `null` stands for a value that was not a number (NaN, inf).
    pub values: Vec<Option<f64>>,
    pub baseline: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub passed: bool,
    pub failures: Vec<String>,
    /// What the numbers show, independent of what the executor claimed.
    pub direction: Option<Outcome>,
    pub mean: Option<f64>,
    pub delta: Option<f64>,
}

pub fn evaluate(checks: &Checks, result: &Value, claimed: Outcome) -> Verdict {
    let mut failures = Vec::new();
    let result: ReportedResult = match serde_json::from_value(result.clone()) {
        Ok(result) => result,
        Err(error) => {
            return Verdict {
                passed: false,
                failures: vec![format!(
                    "the result file is not in the expected shape: {error}"
                )],
                direction: None,
                mean: None,
                delta: None,
            }
        }
    };
    if let Some(expected) = &checks.metric {
        match &result.metric {
            Some(metric) if metric == expected => {}
            Some(metric) => failures.push(format!(
                "reports `{metric}`, but the epic is judged on `{expected}`"
            )),
            None => failures.push(format!("does not name its metric (expected `{expected}`)")),
        }
    }
    let finite: Vec<f64> = result
        .values
        .iter()
        .filter_map(|value| value.filter(|value| value.is_finite()))
        .collect();
    if checks.require_finite && finite.len() != result.values.len() {
        failures.push(format!(
            "{} of {} seed values are not finite numbers",
            result.values.len() - finite.len(),
            result.values.len()
        ));
    }
    if finite.len() < checks.min_seeds {
        failures.push(format!(
            "{} seed(s) reported, {} required",
            finite.len(),
            checks.min_seeds
        ));
    }
    let mean = (!finite.is_empty()).then(|| finite.iter().sum::<f64>() / finite.len() as f64);
    let delta = match (mean, result.baseline) {
        (Some(mean), Some(baseline)) if baseline.is_finite() => Some(if checks.higher_is_better {
            mean - baseline
        } else {
            baseline - mean
        }),
        (_, Some(_)) => {
            failures.push("the baseline value is not a finite number".into());
            None
        }
        (_, None) => {
            failures.push("no baseline value to compare against".into());
            None
        }
    };
    let noise = checks.noise_floor.unwrap_or(0.0).abs();
    let direction = delta.map(|delta| {
        let needed = noise.max(checks.min_delta.unwrap_or(0.0));
        if delta > needed {
            Outcome::Supports
        } else if delta < -noise || (delta <= 0.0 && noise == 0.0) {
            Outcome::Refutes
        } else {
            Outcome::Inconclusive
        }
    });
    if let Some(direction) = direction {
        if direction != claimed {
            failures.push(format!(
                "reported as {claimed}, but the numbers read as {direction}"
            ));
        }
    }
    Verdict {
        passed: failures.is_empty(),
        failures,
        direction,
        mean,
        delta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn checks() -> Checks {
        Checks {
            metric: Some("success_rate".into()),
            noise_floor: Some(0.02),
            ..Checks::default()
        }
    }

    #[test]
    fn a_clear_gain_over_three_seeds_passes_as_support() {
        let verdict = evaluate(
            &checks(),
            &json!({"metric": "success_rate", "values": [0.62, 0.64, 0.63], "baseline": 0.55}),
            Outcome::Supports,
        );
        assert!(verdict.passed, "{:?}", verdict.failures);
        assert_eq!(verdict.direction, Some(Outcome::Supports));
    }

    #[test]
    fn a_gain_inside_the_noise_floor_reads_inconclusive() {
        let verdict = evaluate(
            &checks(),
            &json!({"metric": "success_rate", "values": [0.56, 0.56, 0.56], "baseline": 0.55}),
            Outcome::Supports,
        );
        assert!(!verdict.passed);
        assert_eq!(verdict.direction, Some(Outcome::Inconclusive));
        assert!(verdict.failures[0].contains("inconclusive"));
    }

    #[test]
    fn a_clean_negative_result_passes_when_reported_as_refuting() {
        let verdict = evaluate(
            &checks(),
            &json!({"metric": "success_rate", "values": [0.40, 0.42, 0.41], "baseline": 0.55}),
            Outcome::Refutes,
        );
        assert!(verdict.passed);
        assert_eq!(verdict.direction, Some(Outcome::Refutes));
    }

    #[test]
    fn nan_too_few_seeds_and_a_wrong_metric_each_fail_with_their_reason() {
        let verdict = evaluate(
            &checks(),
            &json!({"metric": "loss", "values": [0.7, null], "baseline": 0.55}),
            Outcome::Supports,
        );
        assert!(!verdict.passed);
        let text = verdict.failures.join("; ");
        assert!(text.contains("`loss`"), "{text}");
        assert!(text.contains("not finite"), "{text}");
        assert!(text.contains("1 seed(s) reported, 3 required"), "{text}");
    }

    #[test]
    fn lower_is_better_metrics_flip_the_comparison() {
        let checks = Checks {
            higher_is_better: false,
            min_seeds: 1,
            ..Checks::default()
        };
        let verdict = evaluate(
            &checks,
            &json!({"values": [1.8], "baseline": 2.0}),
            Outcome::Supports,
        );
        assert!(verdict.passed, "{:?}", verdict.failures);
    }

    #[test]
    fn epic_overrides_replace_only_the_fields_they_name() {
        let merged = Checks::merged(
            &json!({"minSeeds": 5, "noiseFloor": 0.01}),
            &json!({"minSeeds": 2}),
        );
        assert_eq!(merged.min_seeds, 2);
        assert_eq!(merged.noise_floor, Some(0.01));
        assert!(merged.higher_is_better && merged.require_finite);
    }
}
