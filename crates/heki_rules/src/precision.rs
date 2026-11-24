//! The published precision of the rules (docs/precision.md): the certification of each exact
//! rule, and the latest measurement of each measured rule, in the order the tables list them.
//!
//! - `heki rule --output-format json` prints a rule's certification and measurement with it.
//! - `scripts/build_docs.py` writes the two tables of docs/precision.md from these lists (through
//!   the hidden `heki dump-precision`) and copies each rule's measurement into its page; it fails
//!   when an exact rule has no certification free of false claims.

use schemars::JsonSchema;
use serde::{Serialize, Serializer};

/// The certification of an exact rule (docs/precision.md, "How a rule becomes exact").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Certification {
    pub rule: &'static str,
    /// The release that certified it.
    pub release: &'static str,
    /// Every report on the development corpus, checked by hand.
    pub census: Checked,
    /// Every report of one run on the frozen holdout corpus.
    pub holdout: Checked,
}

/// Reports checked by hand, the false claims found among them, and the harm of the right ones
/// where the run classed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Checked {
    pub reports: u32,
    pub false_claims: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harm: Option<Harm>,
}

/// The right reports of a run by class (docs/precision.md): confirmed, plausible, and benign, and
/// those the run left without a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Harm {
    pub confirmed: u32,
    pub plausible: u32,
    pub benign: u32,
    pub unclassified: u32,
}

/// A sample of a rule's reports, each given one class (docs/precision.md): the problem is real
/// and observable (confirmed), right with harm at realistic sizes (plausible), right but
/// deliberate or harmless (benign), or wrong about the code (false).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    pub rule: &'static str,
    /// The round of the measurement protocol.
    pub round: u32,
    /// The date of the build the round measured.
    pub date: &'static str,
    pub confirmed: u32,
    pub plausible: u32,
    pub benign: u32,
    pub false_claims: u32,
}

/// The quantile of the standard normal distribution for a two-sided 95% interval.
const Z_95: f64 = 1.96;

/// `part / whole` as a whole percentage, rounded to the nearest (half up).
fn percent(part: u32, whole: u32) -> u32 {
    (100 * part + whole / 2) / whole
}

impl Measurement {
    pub fn sample(&self) -> u32 {
        self.right() + self.false_claims
    }

    /// The sampled reports that are right about the code: confirmed, plausible, and benign.
    fn right(&self) -> u32 {
        self.confirmed + self.plausible + self.benign
    }

    /// The share of the sample right about the code, in percent.
    pub fn right_percent(&self) -> u32 {
        percent(self.right(), self.sample())
    }

    /// The 95% Wilson score interval of [`Self::right_percent`], in whole percents.
    pub fn interval_percent(&self) -> [u32; 2] {
        let n = f64::from(self.sample());
        let share = f64::from(self.right()) / n;
        let z2 = Z_95 * Z_95;
        let denominator = 1.0 + z2 / n;
        let center = (share + z2 / (2.0 * n)) / denominator;
        let half = Z_95 * (share * (1.0 - share) / n + z2 / (4.0 * n * n)).sqrt() / denominator;
        // Both bounds lie in 0..=1, so the rounded percents fit.
        let whole = |bound: f64| (bound * 100.0).round() as u32;
        [whole(center - half), whole(center + half)]
    }

    /// The share of the sample whose code does real harm (confirmed and plausible), in percent.
    pub fn real_harm_percent(&self) -> u32 {
        percent(self.confirmed + self.plausible, self.sample())
    }

    /// Whether the share of real harm, exactly, is below `percent`.
    pub fn real_harm_below(&self, percent: u32) -> bool {
        100 * (self.confirmed + self.plausible) < percent * self.sample()
    }

    /// The counts with the shares derived from them, as the tables print them.
    pub fn published(&self) -> PublishedMeasurement {
        PublishedMeasurement {
            rule: self.rule.to_owned(),
            round: self.round,
            date: self.date.to_owned(),
            sample: self.sample(),
            confirmed: self.confirmed,
            plausible: self.plausible,
            benign: self.benign,
            false_claims: self.false_claims,
            right_percent: self.right_percent(),
            interval_percent: self.interval_percent(),
            real_harm_percent: self.real_harm_percent(),
        }
    }
}

/// A measurement as published: the classes of the sampled reports, and the shares derived
/// from them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PublishedMeasurement {
    pub rule: String,
    /// The round of the measurement protocol.
    pub round: u32,
    /// The date of the build the round measured.
    pub date: String,
    /// The reports sampled.
    pub sample: u32,
    /// Reports whose problem is real and observable from the code.
    pub confirmed: u32,
    /// Reports that are right, of code that does harm at realistic sizes.
    pub plausible: u32,
    /// Reports that are right, of code that is deliberate or harmless at realistic sizes.
    pub benign: u32,
    /// Reports that are wrong about the code.
    pub false_claims: u32,
    /// The share right about the code (confirmed, plausible, and benign), in percent.
    pub right_percent: u32,
    /// The 95% Wilson score interval of `right_percent`.
    pub interval_percent: [u32; 2],
    /// The share of real harm (confirmed and plausible), in percent.
    pub real_harm_percent: u32,
}

impl Serialize for Measurement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.published().serialize(serializer)
    }
}

/// Every certified rule, in the order the table lists them: the release that certified it, and
/// its census and holdout run as reports and false claims (docs/precision.md says what each run
/// counted).
const CERTIFIED: &[(&str, &str, [u32; 2], [u32; 2])] = &[
    ("OPT101", "0.2.0", [7, 0], [2, 0]),
    ("OPT102", "0.2.0", [1, 0], [0, 0]),
    ("OPT103", "0.2.0", [128, 0], [188, 0]),
    ("OPT110", "0.2.0", [8, 0], [0, 0]),
    ("OPT111", "0.2.0", [7, 0], [7, 0]),
    ("OPT120", "0.2.0", [70, 0], [33, 0]),
    ("OPT121", "0.2.0", [12, 0], [5, 0]),
    ("OPT130", "0.2.0", [1, 0], [0, 0]),
    ("OPT113", "0.2.0", [8, 0], [6, 0]),
    ("EFF101", "0.2.0", [287, 0], [152, 0]),
    ("EFF103", "0.2.0", [103, 0], [122, 0]),
    ("DATA101", "0.2.0", [6, 0], [0, 0]),
    ("MUT110", "0.2.0", [5, 0], [2, 0]),
    ("MUT114", "0.2.0", [5, 0], [5, 0]),
    ("MUT101", "0.2.4", [26, 0], [1, 0]),
    ("MUT111", "0.2.4", [60, 0], [2, 0]),
    ("MUT119", "0.2.5", [23, 0], [2, 0]),
];

/// The harm of the exact reports of a certification's census and holdout run, for the rules whose
/// runs classed it: confirmed, plausible, benign, and not classed, of the census and of the
/// holdout run.
const CERTIFIED_HARM: &[(&str, [u32; 4], [u32; 4])] = &[
    ("OPT101", [0, 0, 6, 1], [0, 0, 2, 0]),
    ("OPT102", [0, 0, 1, 0], [0, 0, 0, 0]),
    ("OPT103", [0, 0, 124, 4], [0, 0, 188, 0]),
    ("OPT110", [0, 0, 7, 1], [0, 0, 0, 0]),
    ("OPT111", [0, 0, 7, 0], [0, 0, 7, 0]),
    ("OPT120", [0, 2, 52, 16], [0, 1, 32, 0]),
    ("OPT121", [0, 0, 10, 2], [0, 0, 5, 0]),
    ("OPT130", [0, 0, 1, 0], [0, 0, 0, 0]),
    ("OPT113", [0, 2, 6, 0], [0, 0, 6, 0]),
];

/// The latest measurement of each measured rule, in the order the table lists them: the round,
/// the date of the build it measured, and the sampled reports as confirmed, plausible, benign,
/// and false (docs/precision.md says how the reviewers of each round classed them).
const MEASURED: &[(&str, u32, &str, [u32; 4])] = &[
    ("EFF101", 3, "2026-11-21", [3, 13, 25, 9]),
    ("EFF102", 3, "2026-11-21", [0, 2, 25, 23]),
    ("EFF103", 3, "2026-11-21", [0, 8, 29, 13]),
    ("DATA101", 3, "2026-11-21", [0, 3, 17, 8]),
    ("MUT107", 3, "2026-11-21", [1, 1, 37, 11]),
    ("MUT110", 3, "2026-11-21", [0, 0, 4, 0]),
    ("MUT202", 3, "2026-11-21", [2, 1, 11, 6]),
    ("OPT101", 3, "2026-11-21", [0, 0, 7, 2]),
    ("OPT102", 3, "2026-11-21", [0, 1, 3, 2]),
    ("OPT103", 3, "2026-11-21", [0, 2, 44, 4]),
    ("OPT110", 3, "2026-11-21", [0, 3, 36, 11]),
    ("OPT111", 3, "2026-11-21", [0, 2, 42, 6]),
    ("OPT112", 3, "2026-11-21", [0, 2, 43, 5]),
    ("OPT113", 3, "2026-11-21", [0, 11, 35, 4]),
    ("OPT120", 3, "2026-11-21", [0, 2, 45, 3]),
    ("OPT121", 3, "2026-11-21", [0, 1, 41, 8]),
    ("OPT130", 3, "2026-11-21", [0, 2, 27, 1]),
];

/// Every certification, in the order the table lists them.
pub fn certifications() -> impl Iterator<Item = Certification> {
    CERTIFIED.iter().map(|&(rule, release, census, holdout)| {
        let harm = CERTIFIED_HARM.iter().find(|(code, ..)| *code == rule);
        let classed = |[confirmed, plausible, benign, unclassified]: [u32; 4]| Harm {
            confirmed,
            plausible,
            benign,
            unclassified,
        };
        let checked = |[reports, false_claims]: [u32; 2], harm: Option<[u32; 4]>| Checked {
            reports,
            false_claims,
            harm: harm.map(classed),
        };
        Certification {
            rule,
            release,
            census: checked(census, harm.map(|h| h.1)),
            holdout: checked(holdout, harm.map(|h| h.2)),
        }
    })
}

/// Every measurement, in the order the table lists them.
pub fn measurements() -> impl Iterator<Item = Measurement> {
    MEASURED.iter().map(
        |&(rule, round, date, [confirmed, plausible, benign, false_claims])| Measurement {
            rule,
            round,
            date,
            confirmed,
            plausible,
            benign,
            false_claims,
        },
    )
}

/// The certification of the rule `code`, if it is certified.
pub fn certification(code: &str) -> Option<Certification> {
    certifications().find(|c| c.rule == code)
}

/// The latest measurement of the rule `code`, if it was measured.
pub fn measurement(code: &str) -> Option<Measurement> {
    measurements().find(|m| m.rule == code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find_rule;

    #[test]
    fn the_classed_harm_counts_every_right_report_of_its_run() {
        for (code, ..) in CERTIFIED_HARM {
            let c = certification(code).unwrap_or_else(|| panic!("{code}: not certified"));
            for run in [c.census, c.holdout] {
                let h = run.harm.unwrap();
                let classed = h.confirmed + h.plausible + h.benign + h.unclassified;
                assert_eq!(classed, run.reports - run.false_claims, "{code}");
            }
        }
    }

    #[test]
    fn every_entry_names_a_rule_once() {
        let certified: Vec<&str> = certifications().map(|c| c.rule).collect();
        let measured: Vec<&str> = measurements().map(|m| m.rule).collect();
        for list in [&certified, &measured] {
            for (i, code) in list.iter().enumerate() {
                assert!(find_rule(code).is_some(), "{code}");
                assert!(!list[i + 1..].contains(code), "{code} twice");
            }
        }
    }

    fn sampled(classes: [u32; 4]) -> Measurement {
        let [confirmed, plausible, benign, false_claims] = classes;
        Measurement {
            rule: "X",
            round: 3,
            date: "2026-11-21",
            confirmed,
            plausible,
            benign,
            false_claims,
        }
    }

    #[test]
    fn shares_are_rounded_and_the_interval_is_wilsons() {
        for (classes, right, interval, harm) in [
            ([3, 13, 25, 9], 82, [69, 90], 32),
            ([0, 3, 17, 8], 71, [53, 85], 11),
            ([0, 0, 4, 0], 100, [51, 100], 0),
            ([0, 1, 3, 2], 67, [30, 90], 17),
            ([0, 2, 27, 1], 97, [83, 99], 7),
        ] {
            let m = sampled(classes);
            assert_eq!(m.right_percent(), right, "{classes:?}");
            assert_eq!(m.interval_percent(), interval, "{classes:?}");
            assert_eq!(m.real_harm_percent(), harm, "{classes:?}");
        }
        // 3 of 28 is 10.7%, not below 10%; 2 of 20 is exactly 10%, not below either.
        assert!(!sampled([0, 3, 17, 8]).real_harm_below(10));
        assert!(!sampled([0, 2, 18, 0]).real_harm_below(10));
        assert!(sampled([1, 1, 37, 11]).real_harm_below(10));
    }
}
