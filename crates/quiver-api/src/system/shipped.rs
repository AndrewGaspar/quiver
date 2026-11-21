//! The privileged pieces Quiver ships as static files (`contrib/`), embedded in this build so
//! the check can compare versions and the script can verify the installed copies' hashes.

use sha2::{Digest, Sha256};

/// One shipped file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shipped {
    /// Its path under `contrib/` (in the checkout, and under the installed `<libdir>/contrib`).
    pub src: &'static str,
    /// Where `quiver system setup` installs it (a per-user install's admin location).
    pub dest: &'static str,
    /// Where a system package installs it, when that differs (udev and sysusers files go to
    /// `/usr/lib`; the ufw profile has only `/etc`).
    pub package: Option<&'static str>,
    /// The marker line's prefix; the version number follows it ("quiver-lab-pad rule v1").
    pub marker: &'static str,
    pub text: &'static str,
}

impl Shipped {
    /// The version this build ships.
    pub fn version(&self) -> u32 {
        version(self.text, self.marker).expect("shipped files carry their marker")
    }

    pub fn sha256(&self) -> String {
        sha256(self.text.as_bytes())
    }

    /// The file's name (`72-quiver-lab-pad.rules`).
    pub fn file_name(&self) -> &'static str {
        self.dest.rsplit('/').next().unwrap_or(self.dest)
    }
}

pub const SYSUSERS: Shipped = Shipped {
    src: "sysusers/quiver.conf",
    dest: "/etc/sysusers.d/quiver.conf",
    package: Some("/usr/lib/sysusers.d/quiver.conf"),
    marker: "quiver-sysusers v",
    text: include_str!("../../../../contrib/sysusers/quiver.conf"),
};

pub const LAB_RULE: Shipped = Shipped {
    src: "lab/72-quiver-lab-pad.rules",
    dest: "/etc/udev/rules.d/72-quiver-lab-pad.rules",
    package: Some("/usr/lib/udev/rules.d/72-quiver-lab-pad.rules"),
    marker: "quiver-lab-pad rule v",
    text: include_str!("../../../../contrib/lab/72-quiver-lab-pad.rules"),
};

pub const STREAM_RULE: Shipped = Shipped {
    src: "stream/72-quiver-stream-pad.rules",
    dest: "/etc/udev/rules.d/72-quiver-stream-pad.rules",
    package: Some("/usr/lib/udev/rules.d/72-quiver-stream-pad.rules"),
    marker: "quiver-stream-pad rule v",
    text: include_str!("../../../../contrib/stream/72-quiver-stream-pad.rules"),
};

pub const UINPUT_RULE: Shipped = Shipped {
    src: "udev/72-quiver-uinput.rules",
    dest: "/etc/udev/rules.d/72-quiver-uinput.rules",
    package: Some("/usr/lib/udev/rules.d/72-quiver-uinput.rules"),
    marker: "quiver-uinput rule v",
    text: include_str!("../../../../contrib/udev/72-quiver-uinput.rules"),
};

pub const UFW_PROFILES: Shipped = Shipped {
    src: "ufw/quiver",
    dest: "/etc/ufw/applications.d/quiver",
    package: None,
    marker: "quiver-ufw profiles v",
    text: include_str!("../../../../contrib/ufw/quiver"),
};

pub const FIREWALLD_MOONLIGHT: Shipped = Shipped {
    src: "firewalld/quiver-moonlight.xml",
    dest: "/etc/firewalld/services/quiver-moonlight.xml",
    package: Some("/usr/lib/firewalld/services/quiver-moonlight.xml"),
    marker: "quiver-firewalld service v",
    text: include_str!("../../../../contrib/firewalld/quiver-moonlight.xml"),
};

pub const FIREWALLD_REMOTE: Shipped = Shipped {
    src: "firewalld/quiver-remote.xml",
    dest: "/etc/firewalld/services/quiver-remote.xml",
    package: Some("/usr/lib/firewalld/services/quiver-remote.xml"),
    marker: "quiver-firewalld service v",
    text: include_str!("../../../../contrib/firewalld/quiver-remote.xml"),
};

/// Every shipped file, for the source check and `--undo`.
pub const ALL: [Shipped; 7] = [SYSUSERS, LAB_RULE, STREAM_RULE, UINPUT_RULE, UFW_PROFILES, FIREWALLD_MOONLIGHT, FIREWALLD_REMOTE];

/// The groups (name, gid) `contrib/sysusers/quiver.conf` makes.
pub const LAB_GROUP: (&str, u32) = ("quiver-lab-pad", 105000);
pub const STREAM_GROUP: (&str, u32) = ("quiver-stream-pad", 105001);
pub const UINPUT_GROUP: (&str, u32) = ("quiver-uinput", 105002);

/// The version after `marker` in `text` ("…rule v3" → 3), if the marker is there.
pub fn version(text: &str, marker: &str) -> Option<u32> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

pub fn sha256(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// The shipped file installed at `path` (its name), for `--undo`.
pub fn by_dest_name(path: &str) -> Option<Shipped> {
    let name = path.rsplit('/').next()?;
    ALL.into_iter().find(|s| s.file_name() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_has_its_marker_and_version() {
        for s in ALL {
            assert_eq!(s.version(), 1, "{}", s.src);
            assert!(s.text.lines().next().unwrap().contains(s.marker) || s.text.lines().nth(1).unwrap().contains(s.marker), "{}: marker near the top", s.src);
            assert_eq!(by_dest_name(s.dest).map(|x| x.src), Some(s.src));
        }
        assert_eq!(version("# quiver-lab-pad rule v12 (x)", "quiver-lab-pad rule v"), Some(12));
        assert_eq!(version("# something else", "quiver-lab-pad rule v"), None);
    }

    #[test]
    fn the_groups_are_in_the_sysusers_file() {
        for (g, gid) in [LAB_GROUP, STREAM_GROUP, UINPUT_GROUP] {
            assert!(SYSUSERS.text.lines().any(|l| l == format!("g {g} {gid}")), "{g}");
        }
        assert!(LAB_RULE.text.contains("GROUP:=\"quiver-lab-pad\""));
        assert!(STREAM_RULE.text.contains("GROUP:=\"quiver-stream-pad\""));
        assert!(UINPUT_RULE.text.contains("GROUP=\"quiver-uinput\""));
    }

    #[test]
    fn the_uinput_rule_touches_uinput_only() {
        let body: Vec<&str> = UINPUT_RULE.text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')).collect();
        assert_eq!(body.len(), 1, "one rule line");
        let l = body[0];
        for want in ["KERNEL==\"uinput\"", "SUBSYSTEM==\"misc\"", "MODE=\"0660\"", "static_node=uinput"] {
            assert!(l.contains(want), "{want}");
        }
        // Never a tag that would hand it to a seat, never "other" access, never a member group.
        assert!(!l.contains("uaccess") && !l.contains("0666") && !l.contains("OWNER"));
    }

    #[test]
    fn profiles_match_the_ports() {
        // The ufw profile and firewalld services carry the same ports as the code.
        let ml = crate::moonlight::ports(crate::moonlight::DEFAULT_PORT);
        let tcp: Vec<String> = ml.iter().filter(|p| p.protocol == "tcp").map(|p| p.port.to_string()).collect();
        assert!(UFW_PROFILES.text.contains(&format!("ports={}/tcp|47998:48000/udp|5353/udp", tcp.join(","))));
        let last = crate::remote::STREAM_PORT + crate::remote::STREAM_PORTS - 1;
        assert!(UFW_PROFILES.text.contains(&format!("ports={}/tcp|{}:{last}/udp", quiver_remote::DEFAULT_PORT, crate::remote::STREAM_PORT)));
        assert!(FIREWALLD_REMOTE.text.contains(&format!("port=\"{}-{last}\"", crate::remote::STREAM_PORT)));
        for p in tcp {
            assert!(FIREWALLD_MOONLIGHT.text.contains(&format!("protocol=\"tcp\" port=\"{p}\"")));
        }
    }
}
