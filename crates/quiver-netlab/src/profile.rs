//! Profiles: one realistic network condition each, as TOML embedded in the binary.
//!
//! A profile file has a `[both]` table that applies to both directions and `[to_client]` /
//! `[to_server]` tables on top of it, each a [`LinkPatch`]. "To client" is server → client:
//! the direction the stream's video travels. Every profile lists where its numbers come from.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::ns::Dir;
use crate::shape::{LinkPatch, LinkShape};

/// A profile's TOML form.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    name: String,
    summary: String,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    both: LinkPatch,
    #[serde(default)]
    to_client: LinkPatch,
    #[serde(default)]
    to_server: LinkPatch,
}

/// A resolved profile: both directions' shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub summary: String,
    pub sources: Vec<String>,
    pub to_client: LinkShape,
    pub to_server: LinkShape,
}

impl Profile {
    pub fn parse(text: &str) -> Result<Profile> {
        let f: ProfileFile = toml::from_str(text)?;
        let base = LinkShape::default().patched(&f.both);
        Ok(Profile {
            to_client: base.patched(&f.to_client),
            to_server: base.patched(&f.to_server),
            name: f.name,
            summary: f.summary,
            sources: f.sources,
        })
    }

    pub fn shape(&self, dir: Dir) -> &LinkShape {
        match dir {
            Dir::ToClient => &self.to_client,
            Dir::ToServer => &self.to_server,
        }
    }

    /// The round trip the delays alone make, in ms.
    pub fn base_rtt_ms(&self) -> f64 {
        self.to_client.delay_ms + self.to_server.delay_ms
    }

    /// An unshaped link, for tests and as a neutral start.
    pub fn open() -> Profile {
        Profile {
            name: "open".into(),
            summary: "No shaping at all: the bare veths and router.".into(),
            sources: vec![],
            to_client: LinkShape::default(),
            to_server: LinkShape::default(),
        }
    }
}

macro_rules! embedded {
    ($($name:literal),* $(,)?) => {
        /// The built-in profiles, by name, in the order `list` shows them.
        pub const BUILTIN: &[(&str, &str)] = &[
            $( ($name, include_str!(concat!("../profiles/", $name, ".toml"))), )*
        ];
    };
}

embedded!(
    "lan-wired",
    "wifi6-good",
    "wifi-far",
    "wifi-congested",
    "5g",
    "lte",
    "cafe",
    "tailscale-relay",
    "cross-country",
    "transatlantic",
    "bufferbloat-cable",
    "pathological",
);

/// Every built-in profile, parsed.
pub fn builtin() -> Vec<Profile> {
    BUILTIN.iter().map(|(n, t)| Profile::parse(t).unwrap_or_else(|e| panic!("profile {n}: {e}"))).collect()
}

/// A profile by built-in name, or from a `.toml` path.
pub fn load(name_or_path: &str) -> Result<Profile> {
    if name_or_path == "open" {
        return Ok(Profile::open());
    }
    if let Some((_, text)) = BUILTIN.iter().find(|(n, _)| *n == name_or_path) {
        return Profile::parse(text);
    }
    if name_or_path.ends_with(".toml") || Path::new(name_or_path).exists() {
        let text = std::fs::read_to_string(name_or_path).with_context(|| format!("reading {name_or_path}"))?;
        return Profile::parse(&text).with_context(|| format!("parsing {name_or_path}"));
    }
    Err(anyhow!(
        "no profile `{name_or_path}`; built in: {}",
        BUILTIN.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_parses_names_itself_and_cites_sources() {
        for (file, text) in BUILTIN {
            let p = Profile::parse(text).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(&p.name, file);
            assert!(!p.sources.is_empty(), "{file} has no sources");
            assert!(p.to_client.rate_mbit > 0.0 && p.to_server.rate_mbit > 0.0, "{file} is unshaped");
        }
    }

    #[test]
    fn both_applies_under_each_direction() {
        let p = Profile::parse(
            "name = 'x'\nsummary = 's'\n[both]\ndelay_ms = 5\nrate_mbit = 10\n[to_client]\nrate_mbit = 100\n",
        )
        .unwrap();
        assert_eq!(p.to_client.delay_ms, 5.0);
        assert_eq!(p.to_server.delay_ms, 5.0);
        assert_eq!(p.to_client.rate_mbit, 100.0);
        assert_eq!(p.to_server.rate_mbit, 10.0);
    }
}
