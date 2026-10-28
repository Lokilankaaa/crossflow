//! Offline T10 arithmetic for manually annotated EXTERNAL VIDEO, not event logs.
//! cargo run --example visible_metrics -- annotations.json
//! stdout is one JSON report; input/evidence files are read-only. No output path
//! is accepted, so existing reports are never opened or overwritten by this tool.
//! Exit 0 = parsed report (inspect status); exit 2 = invalid invocation/input.
//!
//! Schema is documented by the structs below. Exactly rounds 1..5 per A/B/C;
//! additional rounds require a separately frozen extension, not subset selection.
//! Segment endpoints are the first/last annotated visible update; gaps must cover
//! every consecutive update, sum to end-start, and number updates-1. Only continuous
//! uniform motion excluding intentional/subpixel dwell is eligible. All durations
//! use milliseconds except warmup_seconds. Zero is allowed only for video start,
//! rolling-shutter error with explanation, and counts in the empty template.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Write, path::Path};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Input {
    schema_version: u32,
    template: bool,
    format_notes: String,
    measurement_kind: String,   // must be external_video_visible_updates
    baseline_reference: String, // local evidence path, relative to the input JSON
    randomized_order_reference: String,
    all_rounds_included: bool,
    conditions: Conditions,
    rounds: Vec<Round>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Conditions {
    id: String,
    direction: String, // windows_to_mac or mac_to_windows; one report per condition
    details: String,   // hardware/OS/display/mouse/DPI/polling/acceleration/network/load
    actual_refresh_hz: Option<f64>,
    fixed_refresh: bool,
    refresh_evidence: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Round {
    group: String, // A local, B ShareMouse, C Crossflow; no synthetic trajectory D
    round: u32,    // matched block 1..5
    order: u32,    // unique position 1..15 in the recorded randomized order
    condition_id: String,
    settings_match_frozen: bool,
    actual_refresh_hz: Option<f64>,
    implementation: String, // system/software build and configuration
    warmup_seconds: f64,
    video_reference: String,
    annotation_reference: String, // local file documenting manual annotation
    motion_limitation_notes: String,
    camera_fps: f64,
    error_ms: f64,          // E: bound on the entire interval, NOT on one endpoint
    endpoint_error_ms: f64, // >= half a camera frame, including localization
    rolling_shutter_error_ms: f64,
    error_notes: String, // justify both endpoints, shutter/exposure and other error
    segments: Vec<Segment>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Segment {
    start_ms: f64,
    end_ms: f64,
    visible_update_count: usize,
    continuous_uniform_motion: bool,
    intentional_and_subpixel_dwells_excluded: bool,
    all_updates_annotated: bool,
    motion_evidence: String, // explanation/reference to physical motion in video
    gaps_ms: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Bounds {
    observed: f64,
    lower: f64,
    upper: f64,
}

#[derive(Clone, Debug, Serialize)]
struct RoundMetrics {
    group: String,
    round: u32,
    video_reference: String,
    duration_ms: f64,
    samples: usize,
    p95_ms: Bounds,
    definite_stalls: usize,
    uncertain_stalls: usize,
    stall_rate_percent: Bounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Pass,
    Fail,
    Insufficient,
}

fn positive(n: f64) -> bool {
    n.is_finite() && n > 0.0
}
fn nonnegative(n: f64) -> bool {
    n.is_finite() && n >= 0.0
}
fn text(s: &str) -> bool {
    !s.trim().is_empty()
}

fn percentile(mut samples: Vec<f64>, percent: usize) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() * percent).div_ceil(100) - 1]
}

fn classify(g: f64, e: f64, t: f64) -> (bool, bool) {
    let definite = g - e > 2.0 * t;
    let uncertain = !definite && g + e > 2.0 * t;
    (definite, uncertain)
}

fn round_metrics(round: &Round, t: f64) -> RoundMetrics {
    let gaps: Vec<_> = round
        .segments
        .iter()
        .flat_map(|s| s.gaps_ms.iter().copied())
        .collect();
    let p95 = percentile(gaps.clone(), 95);
    let definite = gaps
        .iter()
        .filter(|&&g| classify(g, round.error_ms, t).0)
        .count();
    let uncertain = gaps
        .iter()
        .filter(|&&g| classify(g, round.error_ms, t).1)
        .count();
    let rate = 100.0 / gaps.len() as f64;
    RoundMetrics {
        group: round.group.clone(),
        round: round.round,
        video_reference: round.video_reference.clone(),
        duration_ms: round.segments.iter().map(|s| s.end_ms - s.start_ms).sum(),
        samples: gaps.len(),
        p95_ms: Bounds {
            observed: p95,
            lower: (p95 - round.error_ms).max(0.0),
            upper: p95 + round.error_ms,
        },
        definite_stalls: definite,
        uncertain_stalls: uncertain,
        stall_rate_percent: Bounds {
            observed: definite as f64 * rate,
            lower: definite as f64 * rate,
            upper: (definite + uncertain) as f64 * rate,
        },
    }
}

fn median_bounds(rounds: &[&RoundMetrics], metric: impl Fn(&RoundMetrics) -> Bounds) -> Bounds {
    let values: Vec<_> = rounds.iter().map(|r| metric(r)).collect();
    Bounds {
        observed: percentile(values.iter().map(|v| v.observed).collect(), 50),
        lower: percentile(values.iter().map(|v| v.lower).collect(), 50),
        upper: percentile(values.iter().map(|v| v.upper).collect(), 50),
    }
}

// Conservative interval comparison. Overlapping uncertainty is not a pass.
fn at_most(c: Bounds, reference: Bounds, factor: f64, allowance: f64) -> Decision {
    if c.upper <= factor * reference.lower + allowance {
        Decision::Pass
    } else if c.lower > factor * reference.upper + allowance {
        Decision::Fail
    } else {
        Decision::Insufficient
    }
}
fn combined(values: &[Decision]) -> Decision {
    if values.contains(&Decision::Fail) {
        Decision::Fail
    } else if values.contains(&Decision::Insufficient) {
        Decision::Insufficient
    } else {
        Decision::Pass
    }
}

fn analyze(input: &Input, evidence_exists: impl Fn(&str) -> bool) -> Value {
    let mut issues = Vec::new();
    let mut require = |valid: bool, message: String| {
        if !valid {
            issues.push(message);
        }
    };
    require(
        input.schema_version == 1 && !input.template,
        "schema must be 1 and input must not be an empty template".into(),
    );
    require(input.measurement_kind == "external_video_visible_updates", "only manually annotated external-video updates are eligible; software logs are not visible data".into());
    require(
        input.all_rounds_included,
        "all rounds, including failures, must be included".into(),
    );
    for (name, path) in [
        ("frozen baseline", &input.baseline_reference),
        ("randomized trial order", &input.randomized_order_reference),
        (
            "actual refresh evidence",
            &input.conditions.refresh_evidence,
        ),
    ] {
        require(
            text(path) && evidence_exists(path),
            format!("missing readable local evidence: {name}"),
        );
    }
    let h = input.conditions.actual_refresh_hz.unwrap_or(0.0);
    let t = 1000.0 / h;
    require(
        positive(h) && positive(t) && (2.0 * t).is_finite() && input.conditions.fixed_refresh,
        "actual fixed refresh rate is unknown/invalid; VRR requires a separate frozen protocol"
            .into(),
    );
    require(
        text(&input.conditions.id) && text(&input.conditions.details),
        "missing common condition identity/details".into(),
    );
    require(
        matches!(
            input.conditions.direction.as_str(),
            "windows_to_mac" | "mac_to_windows"
        ),
        "report must describe one explicit direction".into(),
    );
    require(input.rounds.len() == 15, "T10 baseline requires exactly five rounds per A/B/C; fewer or additional rounds need evidence or a separately frozen extension".into());
    let mut seen = BTreeSet::new();
    let mut orders = BTreeSet::new();
    let mut video_spans: Vec<(&str, f64, f64)> = Vec::new();
    for r in &input.rounds {
        let label = format!("{}{}", r.group, r.round);
        require(
            matches!(r.group.as_str(), "A" | "B" | "C")
                && (1..=5).contains(&r.round)
                && seen.insert((r.group.clone(), r.round)),
            format!("{label}: invalid/duplicate group or paired round (must be A/B/C, 1..5)"),
        );
        require(
            (1..=15).contains(&r.order) && orders.insert(r.order),
            format!("{label}: invalid/duplicate randomized order position"),
        );
        require(
            r.condition_id == input.conditions.id
                && r.settings_match_frozen
                && r.actual_refresh_hz == input.conditions.actual_refresh_hz,
            format!("{label}: not confirmed under the identical frozen conditions/actual refresh"),
        );
        require(
            text(&r.implementation) && text(&r.motion_limitation_notes),
            format!("{label}: missing implementation or manual-motion limitation notes"),
        );
        require(
            r.warmup_seconds.is_finite() && r.warmup_seconds >= 120.0,
            format!("{label}: two-minute warmup not documented"),
        );
        for (name, path) in [
            ("video", &r.video_reference),
            ("annotation", &r.annotation_reference),
        ] {
            require(
                text(path) && evidence_exists(path),
                format!("{label}: missing readable local {name} evidence"),
            );
        }
        let budget = 2.0 * r.endpoint_error_ms + r.rolling_shutter_error_ms;
        require(
            positive(r.camera_fps)
                && positive(r.error_ms)
                && positive(r.endpoint_error_ms)
                && nonnegative(r.rolling_shutter_error_ms)
                && positive(budget)
                && r.endpoint_error_ms >= 500.0 / r.camera_fps
                && r.error_ms >= budget
                && text(&r.error_notes),
            format!(
                "{label}: invalid error budget; E must cover two endpoints (each >= half a camera frame) plus rolling shutter, with explanation"
            ),
        );
        require(
            !r.segments.is_empty(),
            format!("{label}: no continuous-motion segments"),
        );
        let mut total = 0.0;
        for (index, s) in r.segments.iter().enumerate() {
            let duration = s.end_ms - s.start_ms;
            require(
                nonnegative(s.start_ms) && positive(s.end_ms) && positive(duration),
                format!("{label} segment {index}: invalid video range"),
            );
            require(
                s.continuous_uniform_motion
                    && s.intentional_and_subpixel_dwells_excluded
                    && s.all_updates_annotated
                    && text(&s.motion_evidence),
                format!(
                    "{label} segment {index}: uniform motion, complete updates and exclusion of intentional/subpixel dwell not established"
                ),
            );
            let sum: f64 = s.gaps_ms.iter().sum();
            require(
                !s.gaps_ms.is_empty()
                    && s.gaps_ms
                        .iter()
                        .all(|&g| positive(g) && (g + r.error_ms + t).is_finite())
                    && s.visible_update_count.checked_sub(1) == Some(s.gaps_ms.len())
                    && positive(sum)
                    && (sum - duration).abs() <= 0.01,
                format!(
                    "{label} segment {index}: gap count/duration mismatch; consecutive gaps must partition the video range within 0.01 ms rounding tolerance"
                ),
            );
            require(
                !video_spans.iter().any(|&(video, start, end)| {
                    video == r.video_reference && s.start_ms < end && start < s.end_ms
                }),
                format!("{label} segment {index}: reused/overlapping video interval"),
            );
            video_spans.push((&r.video_reference, s.start_ms, s.end_ms));
            total += duration;
        }
        require(
            total.is_finite() && total >= 60_000.0,
            format!("{label}: fewer than 60 seconds of eligible continuous motion"),
        );
    }
    for group in ["A", "B", "C"] {
        require(
            (1..=5).all(|round| seen.contains(&(group.to_owned(), round))),
            format!("{group}: missing paired rounds 1..5"),
        );
        let builds: BTreeSet<_> = input
            .rounds
            .iter()
            .filter(|r| r.group == group)
            .map(|r| &r.implementation)
            .collect();
        require(
            builds.len() == 1,
            format!("{group}: implementation/settings changed between rounds"),
        );
    }
    let mut report = json!({
        "schema_version":1,"case":"T10","status":"insufficient","issues":issues,
        "scope":"arithmetic on supplied manual external-video annotations only; not a full phase-0 decision",
        "limitations":["File existence does not authenticate video, annotation accuracy, completeness or common conditions.","No automatic video analysis, no software event-log substitution, no end-to-end input latency claim.","Uncertainty intervals are deterministic annotation bounds, not statistical confidence intervals."],
        "methods":{"p95":"nearest rank: sorted gap at ceil(0.95*N), one-based","aggregation":"median (third sorted value) of exactly five per-round results; never pooled samples or selected rounds","stall_rate_denominator":"number of annotated consecutive visible-update intervals, not elapsed time","definite_stall":"G-E > 2T","normal":"G+E <= 2T","uncertain":"all other intervals crossing the 2T boundary","comparison":"pass only when worst-case C satisfies best-case reference; overlapping bounds are insufficient","interval_error_floor":"E >= 2*endpoint_error + rolling_shutter_error; endpoint_error >= 500/camera_fps ms","same_direction":"C upper stall-rate bound < paired B lower bound in at least 4 of exactly 5 rounds"},
        "format_notes":input.format_notes
    });
    if !issues.is_empty() {
        return report;
    }
    let mut rounds: Vec<_> = input.rounds.iter().map(|r| round_metrics(r, t)).collect();
    rounds.sort_by(|a, b| a.group.cmp(&b.group).then(a.round.cmp(&b.round)));
    let groups: Vec<Vec<_>> = ["A", "B", "C"]
        .into_iter()
        .map(|g| rounds.iter().filter(|r| r.group == g).collect())
        .collect();
    let p: Vec<_> = groups
        .iter()
        .map(|g| median_bounds(g, |r| r.p95_ms))
        .collect();
    let s: Vec<_> = groups
        .iter()
        .map(|g| median_bounds(g, |r| r.stall_rate_percent))
        .collect();
    let baseline_p95 = at_most(p[2], p[0], 1.0, t);
    let baseline_stall = at_most(s[2], s[0], 1.0, 1.0);
    let mut decisions = vec![baseline_p95, baseline_stall];
    let reproducible = groups[1].iter().filter(|r| r.definite_stalls > 0).count() >= 4;
    let no_stalls = groups[1].iter().all(|r| r.stall_rate_percent.upper == 0.0);
    let b_comparison = if reproducible {
        let improvement = at_most(s[2], s[1], 0.7, 0.0);
        let definite = groups[2]
            .iter()
            .zip(&groups[1])
            .filter(|(c, b)| c.stall_rate_percent.upper < b.stall_rate_percent.lower)
            .count();
        let possible = groups[2]
            .iter()
            .zip(&groups[1])
            .filter(|(c, b)| c.stall_rate_percent.lower < b.stall_rate_percent.upper)
            .count();
        let direction = if definite >= 4 {
            Decision::Pass
        } else if possible < 4 {
            Decision::Fail
        } else {
            Decision::Insufficient
        };
        decisions.extend([improvement, direction]);
        json!({"b_stalls":"reproduced_in_at_least_4_rounds","reduction_at_least_30_percent":improvement,"same_direction_4_of_5":direction,"definitely_improved_rounds":definite,"possibly_improved_rounds":possible,"observed_reduction_percent":100.0*(1.0-s[2].observed/s[1].observed),"reduction_percent_bounds":{"lower":100.0*(1.0-s[2].upper/s[1].lower),"upper":100.0*(1.0-s[2].lower/s[1].upper)}})
    } else if no_stalls {
        let checks = [
            at_most(p[2], p[0], 1.0, 0.0),
            at_most(p[2], p[1], 1.0, 0.0),
            at_most(s[2], s[0], 1.0, 0.0),
            at_most(s[2], s[1], 1.0, 0.0),
        ];
        decisions.extend(checks);
        json!({"b_stalls":"not_observed","reduction_at_least_30_percent":"not_applicable_do_not_claim","noninferior_to_a_and_b":combined(&checks),"follow_up":"reproduce the original problem configuration; this report cannot claim its improvement"})
    } else {
        decisions.push(Decision::Insufficient);
        json!({"b_stalls":"reproducibility_uncertain","reduction_at_least_30_percent":"insufficient"})
    };
    report["status"] = json!(combined(&decisions));
    report["condition"] = json!({"id":input.conditions.id,"direction":input.conditions.direction,"actual_refresh_hz":h,"T_ms":t});
    report["rounds"] = json!(rounds);
    report["five_round_medians"] = json!({"A":{"p95_ms":p[0],"stall_rate_percent":s[0]},"B":{"p95_ms":p[1],"stall_rate_percent":s[1]},"C":{"p95_ms":p[2],"stall_rate_percent":s[2]}});
    report["criteria"] = json!({"C_p95_at_most_A_plus_T":baseline_p95,"C_stall_rate_at_most_A_plus_1_percentage_point":baseline_stall,"B_comparison":b_comparison});
    report
}

fn run() -> Result<Value, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 1 {
        return Err(
            "usage: visible_metrics annotations.json (stdout JSON; no output file is overwritten)"
                .into(),
        );
    }
    let path = Path::new(&args[0]);
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if metadata.len() > 64 * 1024 * 1024 {
        return Err("input exceeds 64 MiB limit".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let input: Input =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid annotation JSON: {e}"))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    Ok(analyze(&input, |reference| {
        let target = base.join(reference);
        target.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
            && std::fs::File::open(target).is_ok()
    }))
}

fn main() {
    let (report, code) = match run() {
        Ok(report) => (report, 0),
        Err(error) => (
            json!({"status":"insufficient","input_error":error,"scope":"T10 annotation arithmetic only"}),
            2,
        ),
    };
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer_pretty(&mut stdout, &report).is_err() || writeln!(stdout).is_err() {
        std::process::exit(2);
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    // Synthetic numbers below are isolated algorithm fixtures, NOT recordings
    // or real Crossflow results. They are never exported as measurement data.
    fn fixture() -> Input {
        let mut input = Input {
            schema_version: 1,
            measurement_kind: "external_video_visible_updates".into(),
            baseline_reference: "baseline".into(),
            randomized_order_reference: "order".into(),
            all_rounds_included: true,
            conditions: Conditions {
                id: "test".into(),
                direction: "windows_to_mac".into(),
                details: "unit fixture".into(),
                actual_refresh_hz: Some(100.0),
                fixed_refresh: true,
                refresh_evidence: "refresh".into(),
            },
            ..Input::default()
        };
        for (g, group) in ["A", "B", "C"].into_iter().enumerate() {
            for round in 1..=5 {
                let gaps = if group == "B" {
                    vec![40.0; 1500]
                } else {
                    vec![10.0; 6000]
                };
                input.rounds.push(Round {
                    group: group.into(),
                    round,
                    order: g as u32 * 5 + round,
                    condition_id: "test".into(),
                    settings_match_frozen: true,
                    actual_refresh_hz: Some(100.0),
                    implementation: format!("{group} fixture"),
                    warmup_seconds: 120.0,
                    video_reference: format!("{group}{round}.video"),
                    annotation_reference: "annotation".into(),
                    motion_limitation_notes: "unit fixture only".into(),
                    camera_fps: 1000.0,
                    error_ms: 1.0,
                    endpoint_error_ms: 0.5,
                    rolling_shutter_error_ms: 0.0,
                    error_notes: "unit fixture global shutter".into(),
                    segments: vec![Segment {
                        start_ms: 0.0,
                        end_ms: 60000.0,
                        visible_update_count: gaps.len() + 1,
                        continuous_uniform_motion: true,
                        intentional_and_subpixel_dwells_excluded: true,
                        all_updates_annotated: true,
                        motion_evidence: "unit fixture only".into(),
                        gaps_ms: gaps,
                    }],
                });
            }
        }
        input
    }
    #[test]
    fn strict_stall_threshold_and_nearest_rank() {
        assert_eq!(classify(21.0, 1.0, 10.0), (false, true));
        assert_eq!(classify(21.001, 1.0, 10.0), (true, false));
        assert_eq!(classify(19.0, 1.0, 10.0), (false, false));
        assert_eq!(classify(19.001, 1.0, 10.0), (false, true));
        assert_eq!(percentile((1..=20).map(f64::from).collect(), 95), 19.0);
    }
    #[test]
    fn complete_fixture_uses_five_round_medians_not_pooled_counts() {
        let mut input = fixture();
        // Two slower rounds have different sample counts. Pooling their samples
        // or averaging their P95 values would not give the required median 10.
        for round in &mut input.rounds {
            if round.group == "A" && round.round >= 4 {
                round.segments[0].gaps_ms = vec![40.0; 1500];
                round.segments[0].visible_update_count = 1501;
            }
        }
        let report = analyze(&input, |_| true);
        assert_eq!(report["status"], "pass");
        assert_eq!(
            report["five_round_medians"]["A"]["p95_ms"]["observed"],
            10.0
        );
        assert_eq!(report["rounds"].as_array().unwrap().len(), 15);
        assert_eq!(
            report["five_round_medians"]["B"]["stall_rate_percent"]["observed"],
            100.0
        );
        assert_eq!(
            report["criteria"]["B_comparison"]["definitely_improved_rounds"],
            5
        );
    }
    #[test]
    fn gaps_coverage_count_motion_and_evidence_are_required() {
        for case in 0..9 {
            let mut input = fixture();
            match case {
                0 => {
                    input.rounds.pop();
                }
                1 => input.rounds.push(input.rounds[0].clone()),
                2 => input.conditions.actual_refresh_hz = None,
                3 => input.rounds[0].settings_match_frozen = false,
                4 => input.rounds[0].segments[0].end_ms = 59999.0,
                5 => input.rounds[0].segments[0].visible_update_count += 1,
                6 => input.rounds[0].segments[0].intentional_and_subpixel_dwells_excluded = false,
                7 => input.rounds[0].error_ms = 0.1,
                _ => input.rounds[1].video_reference = input.rounds[0].video_reference.clone(),
            }
            assert_eq!(
                analyze(&input, |_| true)["status"],
                "insufficient",
                "case {case}"
            );
        }
        assert_eq!(analyze(&fixture(), |_| false)["status"], "insufficient");
    }
    #[test]
    fn nonfinite_or_nonpositive_numbers_cannot_pass() {
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut input = fixture();
            input.rounds[0].segments[0].gaps_ms[0] = value;
            assert_eq!(analyze(&input, |_| true)["status"], "insufficient");
        }
    }
    #[test]
    fn reduction_and_percentage_point_thresholds_include_exact_boundary() {
        let exact = |n| Bounds {
            observed: n,
            lower: n,
            upper: n,
        };
        assert_eq!(at_most(exact(70.0), exact(100.0), 0.7, 0.0), Decision::Pass);
        assert_eq!(
            at_most(exact(70.001), exact(100.0), 0.7, 0.0),
            Decision::Fail
        );
        assert_eq!(at_most(exact(1.5), exact(0.5), 1.0, 1.0), Decision::Pass);
        assert_eq!(at_most(exact(1.501), exact(0.5), 1.0, 1.0), Decision::Fail);
    }
    #[test]
    fn measurement_error_can_prevent_a_pass_and_zero_b_does_not_claim_reduction() {
        let c = Bounds {
            observed: 20.0,
            lower: 19.0,
            upper: 21.0,
        };
        let a = Bounds {
            observed: 10.0,
            lower: 9.0,
            upper: 11.0,
        };
        assert_eq!(at_most(c, a, 1.0, 10.0), Decision::Insufficient);
        let mut input = fixture();
        for r in &mut input.rounds {
            if r.group == "B" {
                r.segments[0].gaps_ms = vec![10.0; 6000];
                r.segments[0].visible_update_count = 6001;
            }
        }
        let report = analyze(&input, |_| true);
        assert_eq!(report["status"], "insufficient");
        assert_eq!(
            report["criteria"]["B_comparison"]["reduction_at_least_30_percent"],
            "not_applicable_do_not_claim"
        );
    }
    #[test]
    fn median_improvement_without_four_consistent_rounds_fails() {
        let mut input = fixture();
        for r in &mut input.rounds {
            if r.group == "C" && r.round >= 4 {
                r.segments[0].gaps_ms = vec![40.0; 1500];
                r.segments[0].visible_update_count = 1501;
            }
        }
        let report = analyze(&input, |_| true);
        assert_eq!(
            report["criteria"]["B_comparison"]["reduction_at_least_30_percent"],
            "pass"
        );
        assert_eq!(
            report["criteria"]["B_comparison"]["same_direction_4_of_5"],
            "fail"
        );
        assert_eq!(report["status"], "fail");
    }
}
