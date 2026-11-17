//! Generated protobuf types for Ubisoft Connect's demux protocol, local cache files,
//! and the game-facing `UplayDll` IPC.
#![allow(clippy::all, clippy::pedantic, non_camel_case_types, rustdoc::all)]

macro_rules! include_proto {
    ($pkg:literal) => {
        include!(concat!(env!("OUT_DIR"), "/", $pkg, ".rs"));
    };
}

pub mod mg {
    pub mod playtime { include_proto!("mg.playtime"); }
    pub mod protocol {
        pub mod demux { include_proto!("mg.protocol.demux"); }
        pub mod ownership { include_proto!("mg.protocol.ownership"); }
        pub mod download { include_proto!("mg.protocol.download"); }
        pub mod download_service { include_proto!("mg.protocol.download_service"); }
        pub mod download_install_state { include_proto!("mg.protocol.download_install_state"); }
        pub mod denuvo_service { include_proto!("mg.protocol.denuvo_service"); }
        pub mod utility { include_proto!("mg.protocol.utility"); }
        pub mod uplaydll { include_proto!("mg.protocol.uplaydll"); }
        pub mod uplayauxdll { include_proto!("mg.protocol.uplayauxdll"); }
        pub mod user_dat_file { include_proto!("mg.protocol.user_dat_file"); }
        pub mod ownership_cache { include_proto!("mg.protocol.ownership_cache"); }
        pub mod cloudsave_service { include_proto!("mg.protocol.cloudsave_service"); }
        pub mod game_starter { include_proto!("mg.protocol.game_starter"); }
    }
}

// Not protobuf: Quiver's own mod SDK protocol (`quiver_mod.h`), JSON over the bridge's pipe.
pub mod modsdk;

pub use mg::playtime;
pub use mg::protocol::*;
