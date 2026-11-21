//! Quiver's network-conditions lab (design/REMOTE.md §8.1).
//!
//! Real kernel qdiscs (netem, tbf, fq_codel, cake) shape a link between two network
//! namespaces, in a user namespace the person owns: no root, no sudo, and nothing touches the
//! host's network. On top of that: [`profile`]s (one realistic condition each), [`scenario`]s
//! (a profile changing over time), a [`probe`] that measures what the shaping delivers, a
//! [`run`]ner that puts a server and a client workload on either side and merges their stats
//! into a [`report`], and a stand-in [`fake`] stream so the whole pipeline works before the
//! real host and client exist.
//!
//! Library users (tests in other crates) start with [`Lab::new`], shape it with a
//! [`link::Shaper`], and either spawn processes on a side ([`Lab::spawn`]) or make sockets
//! there for threads in their own process ([`Lab::udp`], [`Lab::tcp_listen`]).

// pub mod fake;
// pub mod flood;
pub mod link;
// pub mod matrix;
pub mod ns;
// pub mod probe;
pub mod profile;
// pub mod report;
// pub mod run;
pub mod scenario;
pub mod shape;
// pub mod stats;

pub use link::Shaper;
pub use ns::{Dir, Lab, Ns, Side, Unavailable};
pub use profile::Profile;
pub use scenario::Scenario;
pub use shape::LinkShape;

/// A lab for a test, or None (with a note on stderr) when this machine can't make one, so
/// the test can return early instead of failing.
pub fn test_lab() -> Option<Lab> {
    match Lab::new() {
        Ok(lab) => Some(lab),
        Err(e) if e.downcast_ref::<Unavailable>().is_some() => {
            eprintln!("skipping: {e}");
            None
        }
        Err(e) => panic!("lab setup failed: {e:#}"),
    }
}
