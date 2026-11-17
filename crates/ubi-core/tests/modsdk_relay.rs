//! The mod SDK protocol through the whole host-side chain, without Wine: a fake mod speaks
//! pipe frames into the bridge's relay (`ubi_ipc::modrelay`, what `ubi-bridge` runs on the
//! named pipe), which opens a broker mod channel over real TCP to a mod registry (what
//! quiverd's broker hands mod channels to).

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{DuplexStream, duplex};
use ubi_core::modsdk::{LiveOutcome, ModRegistry, value};
use ubi_ipc::BrokerRequest;
use ubi_ipc::modrelay::{self, read_frame, write_frame};
use ubi_proto::modsdk::Values;

struct FakeMod(DuplexStream);

impl FakeMod {
    async fn send(&mut self, v: Value) {
        write_frame(&mut self.0, &serde_json::to_vec(&v).unwrap()).await.unwrap();
    }
    async fn recv(&mut self) -> Value {
        let body = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.0, 1 << 20)).await.unwrap().unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}

async fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..400 {
        if let Some(x) = f() {
            return x;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn mod_to_registry_through_the_relay() {
    let reg = ModRegistry::logging();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(reg.clone().serve_listener(listener));

    let (game, pipe) = duplex(1 << 16);
    let relay = tokio::spawn(async move {
        let connect = || async move {
            let s = tokio::net::TcpStream::connect(addr).await?;
            let req = BrokerRequest::ModChannel {
                uplay_id: 3539,
                account_id: Some("acct".into()),
                session: Some("s-relay".into()),
                profile: None,
                conn: 7,
                client_pid: Some(1234),
            };
            modrelay::open_channel(s, &req).await
        };
        modrelay::relay(pipe, connect, &modrelay::Limits::default()).await
    });
    let mut m = FakeMod(game);
    m.send(json!({"t":"hello","proto":2,"sdk":"fake","mod":"aco-dlss","pid":1234,"caps":["options","metrics"]})).await;
    let w = m.recv().await;
    assert_eq!((w["t"].as_str(), w["session"].as_str()), (Some("welcome"), Some("s-relay")));
    m.send(json!({"t":"options","ids":["fg.enabled"]})).await;
    m.send(json!({"t":"metrics","defs":[{"name":"fg.state","kind":"state","values":[{"value":"on"},{"value":"off"}]}]})).await;
    assert_eq!(m.recv().await["ids"]["fg.state"], 0);
    // Junk on the pipe never reaches quiverd.
    write_frame(&mut m.0, b"\x00\x01garbage").await.unwrap();
    m.send(json!({"t":"m","at":1,"v":{"0":"on"}})).await;
    let c = until(|| reg.connections(Some(3539)).into_iter().find(|c| c.metrics.first().is_some_and(|x| x.value.is_some()))).await;
    assert_eq!((c.client_pid, c.refused), (Some(1234), 0));

    let reg2 = reg.clone();
    let set = tokio::spawn(async move {
        let v: Values = [("fg.enabled".to_owned(), value("0", json!(false)))].into_iter().collect();
        reg2.set_options(3539, "aco-dlss", &v, Duration::from_secs(5)).await
    });
    let s = m.recv().await;
    assert_eq!(s["values"]["fg.enabled"], json!({"text":"0","json":false}));
    m.send(json!({"t":"ack","seq":s["seq"]})).await;
    assert_eq!(set.await.unwrap()["fg.enabled"], LiveOutcome::Applied);

    m.send(json!({"t":"bye"})).await;
    let stats = relay.await.unwrap();
    assert_eq!(stats.surface.as_deref(), Some("aco-dlss"));
    assert_eq!(stats.dropped_invalid, 1);
    until(|| reg.connections(None).is_empty().then_some(())).await;
}
