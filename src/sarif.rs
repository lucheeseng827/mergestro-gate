// SPDX-License-Identifier: Apache-2.0
//! SARIF 2.1.0 output: the report's line-level findings in the format GitHub
//! code scanning (and most other code-review tooling) reads, so survivors show
//! up on the line they are about instead of only in a PR comment.
//!
//! Every lane with a location contributes: surviving mutants (level from their
//! severity), zero-assertion tests, and the slop / security / convention / docs
//! lanes. Paths are the report's own — relative to the `--repo` the gate ran
//! on — so upload from a run whose `--repo` is the repository root.

use serde_json::{json, Value};

use crate::pattern::PatternReport;
use crate::report::GateReport;
use crate::severity::Severity;

/// One rule the results refer to: id, one-line description, default level.
struct Rule {
    id: String,
    description: String,
    level: &'static str,
}

/// Render `report` as a SARIF 2.1.0 log (one run, pretty-printed).
pub fn render(report: &GateReport) -> String {
    let mut rules: Vec<Rule> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let mut rule = |id: &str, description: &str, level: &'static str| {
        if !rules.iter().any(|r| r.id == id) {
            rules.push(Rule {
                id: id.to_string(),
                description: description.to_string(),
                level,
            });
        }
    };

    for (sev, m) in report.ranked_survivors() {
        rule(
            "surviving-mutant",
            "The tests still pass with this mutation applied: no test checks this behaviour.",
            "warning",
        );
        let within = m
            .function
            .as_deref()
            .map(|f| format!(" in `{f}`"))
            .unwrap_or_default();
        results.push(result(
            "surviving-mutant",
            level_for(sev),
            &format!(
                "Tests still pass with `{}` applied{within}. Add a test that fails when \
                 this line's behaviour changes ({} severity).",
                m.description,
                sev.label()
            ),
            &m.file,
            m.line,
            m.column,
        ));
    }

    for z in &report.zero_assertion_tests {
        rule(
            "zero-assertion-test",
            "A test on the changed surface that asserts nothing: it passes whatever the code does.",
            "warning",
        );
        results.push(result(
            "zero-assertion-test",
            "warning",
            &format!(
                "`{}` asserts nothing, so it passes whatever the code does.",
                z.function
            ),
            &z.file,
            z.line,
            0,
        ));
    }

    let lanes: [(&str, &Option<PatternReport>, &'static str); 5] = [
        ("security", &report.security, "warning"),
        ("weakened-tests", &report.weakened_tests, "warning"),
        ("slop", &report.slop, "note"),
        ("convention", &report.convention, "note"),
        ("docs", &report.docs, "note"),
    ];
    for (lane, lane_report, level) in lanes {
        for f in lane_report.iter().flat_map(|r| &r.findings) {
            rule(
                &f.rule,
                &format!("Mergestro Gate {lane} lane: {}", f.rule),
                level,
            );
            results.push(result(&f.rule, level, &f.message, &f.file, f.line, 0));
        }
    }

    let rules: Vec<Value> = rules
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "shortDescription": { "text": r.description },
                "defaultConfiguration": { "level": r.level },
            })
        })
        .collect();
    let log = json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": {
                "name": "Mergestro Gate",
                "version": env!("CARGO_PKG_VERSION"),
                "informationUri": "https://github.com/lucheeseng827/mergestro-gate",
                "rules": rules,
            }},
            "results": results,
        }],
    });
    serde_json::to_string_pretty(&log).expect("a json! value always serialises")
}

/// Code scanning's three levels: a survivor that could hide a real bug is an
/// error; the rest are warnings or notes.
fn level_for(sev: Severity) -> &'static str {
    match sev {
        Severity::Critical | Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// One result. `line == 0` means "the file as a whole" (the docs lane): SARIF
/// regions are 1-based, so no region is emitted rather than an invalid one.
///
/// No `partialFingerprints`: `upload-sarif` computes GitHub's
/// `primaryLocationLineHash` from the source line when they are absent, which
/// is distinct per location and follows a finding when code above it moves.
/// Anything we could compute here is weaker — survivors read back from
/// `missed.txt` carry no function name, so two `replace + with -` in one file
/// would share an identity and code scanning would merge them.
fn result(rule_id: &str, level: &str, message: &str, file: &str, line: u32, column: u32) -> Value {
    let mut physical = json!({ "artifactLocation": { "uri": file } });
    if line > 0 {
        let mut region = json!({ "startLine": line });
        if column > 0 {
            region["startColumn"] = json!(column);
        }
        physical["region"] = region;
    }
    json!({
        "ruleId": rule_id,
        "level": level,
        "message": { "text": message },
        "locations": [{ "physicalLocation": physical }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::PatternFinding;
    use crate::report::{Mutant, ZeroAssertionFinding};

    fn sarif(report: &GateReport) -> Value {
        serde_json::from_str(&render(report)).expect("render emits valid JSON")
    }

    #[test]
    fn a_clean_run_is_a_valid_log_with_no_results() {
        let v = sarif(&GateReport::new("main", "HEAD"));
        assert_eq!(v["version"], "2.1.0");
        assert_eq!(v["runs"][0]["tool"]["driver"]["name"], "Mergestro Gate");
        assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn survivors_land_on_their_line_with_a_level_from_severity() {
        let mut r = GateReport::new("main", "HEAD");
        r.survivors = vec![
            Mutant::parse_name("src/auth.rs:9:5: replace > with >=").unwrap(), // critical
            Mutant::parse_name("src/util.rs:5:7: replace + with -").unwrap(),  // medium
        ];
        let v = sarif(&r);
        let results = v["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        let first = &results[0]; // ranked most severe first
        assert_eq!(first["ruleId"], "surviving-mutant");
        assert_eq!(first["level"], "error");
        let loc = &first["locations"][0]["physicalLocation"];
        assert_eq!(loc["artifactLocation"]["uri"], "src/auth.rs");
        assert_eq!(loc["region"]["startLine"], 9);
        assert_eq!(loc["region"]["startColumn"], 5);
        assert_eq!(results[1]["level"], "warning");
        // One rule entry, however many results use it.
        let rules = v["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn every_lane_with_a_location_contributes_and_line_zero_is_file_level() {
        let mut r = GateReport::new("main", "HEAD");
        r.zero_assertion_tests = vec![ZeroAssertionFinding {
            file: "src/x.rs".into(),
            line: 12,
            function: "it_works".into(),
        }];
        r.security = Some(PatternReport::from_findings(vec![PatternFinding {
            rule: "hardcoded-secret".into(),
            file: "src/cfg.rs".into(),
            line: 3,
            message: "a secret literal".into(),
            weight: 10,
        }]));
        r.docs = Some(PatternReport::from_findings(vec![PatternFinding {
            rule: "docs-missing-readme".into(),
            file: "a/src/lib.rs".into(),
            line: 0,
            message: "module a has code changes but no README.md".into(),
            weight: 1,
        }]));
        let v = sarif(&r);
        let results = v["runs"][0]["results"].as_array().unwrap();
        let ids: Vec<&str> = results
            .iter()
            .map(|x| x["ruleId"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "zero-assertion-test",
                "hardcoded-secret",
                "docs-missing-readme"
            ]
        );
        let docs = &results[2]["locations"][0]["physicalLocation"];
        assert!(
            docs.get("region").is_none(),
            "line 0 must not become startLine 0"
        );
    }

    #[test]
    fn identical_mutations_in_one_file_stay_separate_results() {
        // Survivors read from missed.txt have no function name. Two `+ -> -` in
        // one file must not share an identity, or code scanning merges them:
        // no fingerprint of ours, only the two locations, left to upload-sarif.
        let mut r = GateReport::new("main", "HEAD");
        r.survivors = vec![
            Mutant::parse_name("src/u.rs:5:7: replace + with -").unwrap(),
            Mutant::parse_name("src/u.rs:9:7: replace + with -").unwrap(),
        ];
        let v = sarif(&r);
        let results = v["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|x| x.get("partialFingerprints").is_none()));
        let lines: Vec<u64> = results
            .iter()
            .map(|x| {
                x["locations"][0]["physicalLocation"]["region"]["startLine"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(lines.len(), 2);
        assert_ne!(lines[0], lines[1]);
    }
}
