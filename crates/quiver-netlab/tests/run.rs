//! The whole pipeline with the stand-in stream: a scenario's timeline, both workloads in
//! their namespaces, their stats merged into the report, and the CLI around it.

use std::process::Command;
use std::sync::atomic::AtomicBool;

use quiver_netlab::Scenario;
use quiver_netlab::run::{RunSpec, run};
use quiver_netlab::test_lab;

const EXE: &str = env!("CARGO_BIN_EXE_quiver-netlab");

const SCENARIO: &str = r#"
name = "test-mix"
summary = "A short mix of everything the timeline does."
base = "cross-country"
duration_s = 9

[both]
delay_ms = 5
jitter_ms = 0
loss = { pct = 0 }

[to_client]
rate_mbit = 40

[[event]]
at_s = 3
kind = "set"
label = "40 → 8 Mbit/s"
to_client = { rate_mbit = 8 }
for_s = 3

[[event]]
at_s = 4
kind = "flood"
proto = "udp"
rate_mbit = 4
dir = "to_client"
for_s = 1

[[event]]
at_s = 7
kind = "roam"
gap_ms = 100
"#;

#[test]
fn a_scenario_runs_both_workloads_and_reports() {
    let Some(lab) = test_lab() else { return };
    let dir = tempfile::tempdir().unwrap();
    let sc = Scenario::parse(SCENARIO).unwrap();
    let spec = RunSpec {
        label: sc.name.clone(),
        scenario: sc,
        server: Some(format!("'{EXE}' fake-stream serve --aimd --mbit 5 --max-mbit 30")),
        client: Some(format!("'{EXE}' fake-stream play --period-ms 250")),
        out_dir: dir.path().to_path_buf(),
        path_probe_pps: 50,
        env: vec![("QUIVER_SHM_ROOT".into(), dir.path().join("shm").display().to_string())],
    };
    let r = run(&lab, &spec, &AtomicBool::new(false)).unwrap();
    let html = r.write(&dir.path().join("report.json")).unwrap();

    assert!(r.client_stats.len() >= 20, "client lines: {}", r.client_stats.len());
    assert!(r.host_stats.len() >= 10, "host lines: {}", r.host_stats.len());
    assert_eq!(r.bad_stats_lines, 0);
    // The rate went down and came back, the flood ran, the client moved.
    let rates: Vec<f64> = r.applied.iter().filter(|a| a.dir == quiver_netlab::Dir::ToClient).map(|a| a.rate_mbit).collect();
    assert!(rates.contains(&8.0) && rates.last() == Some(&40.0), "{rates:?}");
    assert_eq!(r.floods.len(), 1);
    assert!(r.floods[0].delivered_bytes > 100_000, "{:?}", r.floods[0]);
    assert_eq!(r.roams.len(), 1);
    // The sender followed the client to its new address.
    let migrations = r.host_stats.iter().filter_map(|t| t.line.extra("migrations")).fold(0.0, f64::max);
    assert_eq!(migrations, 1.0);
    // Frames flowed after the roam.
    let after = r.client_stats.iter().filter(|t| t.t_ms > 8000.0).map(|t| t.line.client.as_ref().unwrap().frames_received).sum::<u32>();
    assert!(after > 20, "frames after the roam: {after}");
    let s = &r.summary;
    assert!(s.latency_p50_ms.unwrap() > 5.0 && s.latency_p50_ms.unwrap() < 200.0, "{s:?}");
    assert!(s.path_rtt_p50_ms.unwrap() >= 10.0, "{s:?}");
    assert!(s.mean_bitrate_mbit.unwrap() > 1.0);
    assert!(s.recoveries.iter().any(|x| x.step_down), "{:?}", s.recoveries);
    let page = std::fs::read_to_string(html).unwrap();
    assert!(page.contains("<svg") && page.contains("40 → 8 Mbit/s"));
    assert!(!page.contains("<script src"), "the page loads nothing external");
}

#[test]
fn cli_lists_and_runs_with_the_stand_in() {
    let out = Command::new(EXE).arg("list").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    for name in ["lan-wired", "bufferbloat-cable", "pathological", "step-down", "roam", "competing-tcp"] {
        assert!(text.contains(name), "{name} missing from list");
    }
    if test_lab().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let st = Command::new(EXE)
        .args(["run", "--profile", "wifi-far", "--fake", "--duration", "4", "--out"])
        .arg(dir.path())
        .env("QUIVER_SHM_ROOT", dir.path().join("shm"))
        .status()
        .unwrap();
    assert!(st.success());
    let report: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["scenario"]["name"], "wifi-far");
    assert!(report["client_stats"].as_array().unwrap().len() >= 4);
    assert!(dir.path().join("report.html").exists());
    assert!(dir.path().join("client.log").exists());
}

#[test]
fn matrix_writes_a_summary_row_per_item() {
    if test_lab().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let st = Command::new(EXE)
        .args(["matrix", "--profiles", "lan-wired,lte", "--fake", "--duration", "3", "--probe-pps", "10", "--out"])
        .arg(dir.path())
        .env("QUIVER_SHM_ROOT", dir.path().join("shm"))
        .status()
        .unwrap();
    assert!(st.success());
    let rows: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("summary.json")).unwrap()).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for r in rows {
        assert!(r["error"].is_null(), "{r}");
        assert!(r["summary"]["latency_p50_ms"].is_number(), "{r}");
    }
    let md = std::fs::read_to_string(dir.path().join("summary.md")).unwrap();
    assert!(md.contains("| lan-wired | profile |") && md.contains("| lte | profile |"), "{md}");
    assert!(dir.path().join("lte/report.html").exists());
}
