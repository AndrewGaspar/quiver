//! Native Ubisoft Connect client core: authentication, demux protocol, library,
//! and manifest handling.

pub mod auth;
pub mod broker;
pub mod demux;
pub mod launch;
pub mod library;
pub mod manifest;
pub mod store;

pub use ubi_proto as proto;

/// Log in (refreshing if needed) and return an authenticated demux socket.
pub async fn connect(auth: &auth::Auth) -> anyhow::Result<(auth::Session, demux::Demux)> {
    let session = store::current(auth).await?;
    let demux = demux::Demux::connect().await?;
    demux.authenticate(&session.ticket).await?;
    Ok((session, demux))
}
