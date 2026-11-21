//! Every built-in profile, measured: the kernel's shaping has to deliver what the profile
//! (through the model of netem) says. Run with `--nocapture` to see the numbers.

use quiver_netlab::probe::{ProbeOptions, verify};
use quiver_netlab::{profile, test_lab};

#[test]
fn every_profile_delivers_what_it_says() {
    let Some(lab) = test_lab() else { return };
    let mut failures = Vec::new();
    for p in profile::builtin() {
        let v = verify(&lab, &p, &ProbeOptions::default()).unwrap();
        print!("{}", v.render());
        for c in v.checks.iter().filter(|c| !c.ok) {
            failures.push(format!("{}: {} target {} measured {}", p.name, c.what, c.target, c.measured));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
