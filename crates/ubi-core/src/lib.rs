//! Native Ubisoft Connect client core: authentication, demux protocol, library,
//! and manifest handling.

pub mod auth;
pub mod broker;
pub mod cloudsave;
pub mod demux;
pub mod download;
pub mod install;
pub mod launch;
pub mod launcher;
pub mod library;
pub mod manifest;
pub mod online;
pub mod playsession;
pub mod savesync;
pub mod settings;
pub mod store;

pub use ubi_proto as proto;

/// Log in (refreshing if needed) and return an authenticated demux socket.
pub async fn connect(auth: &auth::Auth) -> anyhow::Result<(auth::Session, demux::Demux)> {
    let session = store::current(auth, None).await?;
    let demux = demux::Demux::connect().await?;
    demux.authenticate(&session.ticket).await?;
    Ok((session, demux))
}
