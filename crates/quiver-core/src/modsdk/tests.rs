use std::sync::Mutex;

use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, duplex};

use super::*;

/// Records what the sinks hear.
#[derive(Default)]
struct Rec {
    log: Mutex<Vec<String>>,
    snapshot: Values,
}

impl Rec {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.log.lock().unwrap())
    }
    fn push(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }
}

impl ModOptionsSink for Rec {
    fn snapshot(&self, m: &ModSdkConnection) -> Values {
        self.push(format!("snapshot {}", m.surface));
        self.snapshot.clone()
    }
    fn handles(&self, m: &ModSdkConnection) {
        self.push(format!("handles {:?}", m.options));
    }
    fn changed(&self, _m: &ModSdkConnection, values: &ChangedValues, persist: bool) {
        self.push(format!("changed {values:?} persist={persist}"));
    }
    fn disconnected(&self, m: &ModSdkConnection) {
        self.push(format!("options gone {}", m.surface));
    }
    fn notice(&self, m: &ModSdkConnection, n: &wire::ModNotice) {
        self.push(format!("notice {} {} {} {:?}", m.conn, n.id, n.title, n.buttons.iter().map(|b| b.id.as_str()).collect::<Vec<_>>()));
    }
}

impl ModMetricsSink for Rec {
    fn declared(&self, _m: &ModSdkConnection, metrics: &[DeclaredMetric]) {
        self.push(format!("declared {:?}", metrics.iter().map(|d| (d.index, d.id.as_str())).collect::<Vec<_>>()));
    }
    fn samples(&self, _m: &ModSdkConnection, at: i64, samples: &[Sample]) {
        self.push(format!("samples at={at} {:?}", samples.iter().map(|s| (s.id.as_str(), &s.value)).collect::<Vec<_>>()));
    }
    fn mark(&self, _m: &ModSdkConnection, metric: &DeclaredMetric, at: i64, dur: Option<i64>, label: Option<&str>) {
        self.push(format!("mark {} at={at} dur={dur:?} label={label:?}", metric.id));
    }
    fn stereo(&self, m: &ModSdkConnection) {
        self.push(format!("stereo {:?}", m.stereo));
    }
    fn disconnected(&self, m: &ModSdkConnection) {
        self.push(format!("metrics gone {}", m.surface));
    }
}

/// The mod's end of a channel, as the bridge would relay it: JSON lines.
struct Mod {
    w: DuplexStream,
    r: BufReader<DuplexStream>,
}

impl Mod {
    async fn send(&mut self, v: Value) {
        let mut l = serde_json::to_vec(&v).unwrap();
        l.push(b'\n');
        self.w.write_all(&l).await.unwrap();
    }
    async fn recv(&mut self) -> Value {
        let l = tokio::time::timeout(Duration::from_secs(5), read_line_capped(&mut self.r, 1 << 20)).await.unwrap().unwrap().unwrap();
        serde_json::from_slice(&l).unwrap()
    }
}

fn setup() -> (Arc<ModRegistry>, Arc<Rec>) {
    let rec = Arc::new(Rec {
        snapshot: [("fg.enabled".to_owned(), value("1", json!(true)))].into_iter().collect(),
        ..Default::default()
    });
    (ModRegistry::new(rec.clone(), rec.clone()), rec)
}

fn connect(reg: &Arc<ModRegistry>, product_id: u32) -> (Mod, tokio::task::JoinHandle<Result<()>>) {
    let (to_q, q_in) = duplex(1 << 16);
    let (q_out, from_q) = duplex(1 << 16);
    let reg = reg.clone();
    let origin = ChannelOrigin { product_id, session: Some("s-test".into()), profile: Some("p1".into()), bridge_conn: 1, ..Default::default() };
    let task = tokio::spawn(async move { reg.serve(origin, BufReader::new(q_in), q_out).await });
    (Mod { w: to_q, r: BufReader::new(from_q) }, task)
}

async fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(x) = f() {
            return x;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition never held");
}

#[tokio::test]
async fn stereo_layout_from_the_sdk() {
    let (reg, rec) = setup();
    let (mut m, task) = connect(&reg, 0);
    m.send(json!({"t":"hello","proto":3,"mod":"torchbearer","caps":["options"]})).await;
    assert_eq!(m.recv().await["proto"], 3);
    m.send(json!({"t":"stereo","layout":"sbs"})).await;
    until(|| rec.take().into_iter().find(|l| l.starts_with("stereo")).filter(|l| l == "stereo Some(\"sbs\")")).await;
    assert_eq!(reg.connections(None)[0].stereo.as_deref(), Some("sbs"));
    // The same again: no news. HypXRland's tag spelling and `off` are the same layouts.
    m.send(json!({"t":"stereo","layout":"stereo:sbs"})).await;
    m.send(json!({"t":"stereo","layout":"off"})).await;
    until(|| rec.take().into_iter().find(|l| l == "stereo Some(\"mono\")")).await;
    // Not a layout: refused, unchanged.
    m.send(json!({"t":"stereo","layout":"3d"})).await;
    until(|| (reg.connections(None)[0].refused == 1).then_some(())).await;
    assert_eq!(reg.connections(None)[0].stereo.as_deref(), Some("mono"));
    // Withdrawn: the setting applies again.
    m.send(json!({"t":"stereo"})).await;
    until(|| rec.take().into_iter().find(|l| l == "stereo None")).await;
    m.send(json!({"t":"bye"})).await;
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn notices_and_their_answers() {
    let (reg, rec) = setup();
    let (mut m, task) = connect(&reg, 0);
    m.send(json!({"t":"hello","proto":3,"mod":"aco-dlss","caps":["options"]})).await;
    assert_eq!(m.recv().await["t"], "welcome");
    m.send(json!({"t":"notice","id":"dlss4","title":"DLSS v4 is ready","buttons":[{"id":"keep_v3","label":"Keep v3 for now"},{"id":"notes","label":"Notes"}]})).await;
    let conn = reg.connections(None)[0].conn;
    let line = until(|| rec.take().into_iter().find(|l| l.starts_with("notice"))).await;
    assert_eq!(line, format!("notice {conn} dlss4 DLSS v4 is ready [\"keep_v3\", \"notes\"]"));
    // A second one within the second is over the rate.
    m.send(json!({"t":"notice","id":"again","title":"x"})).await;
    until(|| (reg.connections(None)[0].refused == 1).then_some(())).await;
    // The player's answer goes back to the mod.
    assert!(reg.notice_action(conn, "dlss4", "keep_v3"));
    assert_eq!(m.recv().await, json!({"t":"notice_action","id":"dlss4","button":"keep_v3"}));
    assert!(!reg.notice_action(conn + 100, "dlss4", "keep_v3"), "no such connection");
    m.send(json!({"t":"bye"})).await;
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn handshake_metrics_and_status() {
    let (reg, rec) = setup();
    let (mut m, task) = connect(&reg, 3539);
    m.send(json!({"t":"hello","proto":7,"sdk":"quiver_mod.h 2.0","mod":"aco-dlss","pid":40,"module":"C:\\game\\d3d11.dll",
                  "caps":["metrics","options","rings"]}))
        .await;
    let w = m.recv().await;
    assert_eq!(w["t"], "welcome");
    assert_eq!(w["proto"], 3, "min(theirs, ours)");
    assert_eq!(w["caps"], json!(["options", "metrics"]), "only what both know");
    assert_eq!(w["session"], "s-test");
    assert_eq!(w["profile"], "p1");
    assert_eq!(w["values"]["fg.enabled"], json!({"text":"1","json":true}));
    assert!(w.get("perf").is_none(), "no rings yet, so no session dir");
    assert_eq!(w["limits"]["metrics"], 64);

    m.send(json!({"t":"metrics","defs":[
        {"id":"aco-dlss.fg.state","kind":"state","values":[{"value":"off"},{"value":"on"}]},
        {"id":"aco-dlss.fps.base","kind":"gauge","unit":"fps","label":"Base FPS"},
        {"id":"aco-dlss.frames.real","kind":"counter"},
        {"id":"aco-dlss.upscaler","kind":"text"},
        {"id":"aco-dlss.bench","kind":"event"},
        {"id":"aco-dlss.frame","kind":"frame"},
        {"id":"Bad Name","kind":"gauge"},
        {"id":"other-mod.x","kind":"gauge"},
        {"kind":"gauge"},
    ]}))
    .await;
    let ok = m.recv().await;
    assert_eq!(ok["t"], "metrics_ok");
    assert_eq!(ok["ids"], json!({"aco-dlss.fg.state":1,"aco-dlss.fps.base":2,"aco-dlss.frames.real":3,"aco-dlss.upscaler":4,"aco-dlss.bench":5}), "1-based, like ring records");
    let rejected = ok["rejected"].as_object().unwrap();
    assert!(rejected["aco-dlss.frame"].as_str().unwrap().contains("ring"));
    assert!(rejected["other-mod.x"].as_str().unwrap().contains("namespace"), "only its own namespace");
    assert!(rejected.contains_key("Bad Name") && rejected.contains_key("#8"), "{rejected:?}");

    m.send(json!({"t":"m","ts":1000,"v":{"1":"on","2":79.5,"3":4211,"4":"DLSS K","9":1,"0":2}})).await;
    // Wrong types are refused per value.
    m.send(json!({"t":"m","ts":1001,"v":{"1":"sideways","2":"fast"}})).await;
    m.send(json!({"t":"mark","index":5,"ts":1002,"label":"begin\u{202e}"})).await;
    m.send(json!({"t":"mark","index":2,"ts":1003})).await;
    let st = until(|| reg.connections(Some(3539)).into_iter().next().filter(|c| c.metrics.iter().any(|m| m.marks > 0))).await;
    assert_eq!(st.surface, "aco-dlss");
    assert_eq!(st.module.as_deref(), Some("d3d11.dll"));
    assert_eq!(st.metrics.len(), 5);
    assert_eq!(st.metrics[0].id, "aco-dlss.fg.state");
    assert_eq!(st.metrics[0].value, Some(json!("on")));
    assert_eq!(st.metrics[1].value, Some(json!(79.5)));
    assert_eq!(st.metrics[2].value, Some(json!(4211)));
    assert_eq!(st.metrics[4].value, Some(json!("begin")));
    assert!(st.refused >= 4, "{st:?}");
    assert!(reg.connections(Some(1)).is_empty());

    let log = rec.take();
    assert_eq!(log[0], "snapshot aco-dlss");
    assert!(log[1].starts_with("declared [(1, \"aco-dlss.fg.state\")"), "{log:?}");
    assert!(log[2].contains("at=1000") && log[2].contains("Gauge(79.5)") && log[2].contains("Counter(4211)") && log[2].contains("State(1)"), "{log:?}");
    assert_eq!(log[3], "mark aco-dlss.bench at=1002 dur=None label=Some(\"begin\")");

    m.send(json!({"t":"bye"})).await;
    task.await.unwrap().unwrap();
    assert!(reg.connections(None).is_empty());
    assert_eq!(rec.take(), ["options gone aco-dlss", "metrics gone aco-dlss"]);
}

#[tokio::test]
async fn live_options_are_acked() {
    let (reg, rec) = setup();
    let vals: Values = [
        ("fg.enabled".to_owned(), value("0", json!(false))),
        ("world.npc_density".to_owned(), value("9", json!(9))),
        ("other".to_owned(), value("x", json!("x"))),
    ]
    .into_iter()
    .collect();
    let out = reg.set_options(3539, "aco-dlss", &vals, Duration::from_millis(50)).await;
    assert!(out.values().all(|o| *o == LiveOutcome::NoConnection));

    let (mut m, task) = connect(&reg, 3539);
    m.send(json!({"t":"hello","proto":2,"mod":"aco-dlss","caps":["options","actions"]})).await;
    m.recv().await;
    m.send(json!({"t":"options","ids":["fg.enabled","world.npc_density","bad id"]})).await;
    m.send(json!({"t":"options","ids":["fg.enabled","world.npc_density"]})).await;
    until(|| (reg.live_options(3539, "aco-dlss").len() == 2).then_some(())).await;

    let reg2 = reg.clone();
    let vals2 = vals.clone();
    let push = tokio::spawn(async move { reg2.set_options(3539, "aco-dlss", &vals2, Duration::from_secs(5)).await });
    let set = m.recv().await;
    assert_eq!(set["t"], "set");
    assert_eq!(set["values"].as_object().unwrap().len(), 2, "only what it handles: {set}");
    m.send(json!({"t":"ack","seq":set["seq"],"rejected":{"world.npc_density":"max is 8","other":"not yours"}})).await;
    let out = push.await.unwrap();
    assert_eq!(out["fg.enabled"], LiveOutcome::Applied);
    assert_eq!(out["world.npc_density"], LiveOutcome::Rejected("max is 8".into()));
    assert_eq!(out["other"], LiveOutcome::NotHandled);

    // No ack: a timeout, not "applied".
    let out = reg.set_options(3539, "aco-dlss", &vals, Duration::from_millis(50)).await;
    assert_eq!(out["fg.enabled"], LiveOutcome::Timeout);
    assert_eq!(m.recv().await["t"], "set");

    assert_eq!(reg.invoke_action(3539, "aco-dlss", "fg.toggle", None).unwrap(), 1);
    assert_eq!(m.recv().await, json!({"t":"action","id":"fg.toggle"}));

    m.send(json!({"t":"changed","values":{"fg.enabled":"1\u{0007}","Bad Id":"x"},"persist":false})).await;
    m.send(json!({"t":"changed","values":{"fg.enabled":"1\u{0007}"},"persist":false})).await;
    m.send(json!({"t":"log","level":1,"text":"hello from the mod"})).await;
    m.send(json!({"t":"some_future_message","x":1})).await;
    m.send(json!({"t":"bye"})).await;
    task.await.unwrap().unwrap();
    let log = rec.take();
    assert!(log.contains(&"handles [\"fg.enabled\", \"world.npc_density\"]".to_owned()), "{log:?}");
    assert!(log.contains(&"changed {\"fg.enabled\": \"1\"} persist=false".to_owned()), "{log:?}");
    assert_eq!(log.iter().filter(|l| l.starts_with("changed")).count(), 1);
}

#[tokio::test]
async fn pending_sets_drop_with_the_connection() {
    let (reg, _rec) = setup();
    let (mut m, task) = connect(&reg, 1);
    m.send(json!({"t":"hello","proto":2,"mod":"m","caps":["options"]})).await;
    m.recv().await;
    m.send(json!({"t":"options","ids":["a"]})).await;
    until(|| (!reg.live_options(1, "m").is_empty()).then_some(())).await;
    let reg2 = reg.clone();
    let push = tokio::spawn(async move {
        let v: Values = [("a".to_owned(), value("1", json!(1)))].into_iter().collect();
        reg2.set_options(1, "m", &v, Duration::from_secs(5)).await
    });
    assert_eq!(m.recv().await["t"], "set");
    drop(m);
    let _ = task.await.unwrap();
    assert_eq!(push.await.unwrap()["a"], LiveOutcome::Dropped);
}

#[tokio::test]
async fn refusals() {
    let (reg, _rec) = setup();
    let (mut m, task) = connect(&reg, 1);
    m.send(json!({"t":"hello","proto":2,"mod":"../etc"})).await;
    let bye = m.recv().await;
    assert_eq!(bye["t"], "bye");
    assert!(task.await.unwrap().is_err());
    assert!(reg.connections(None).is_empty());

    // Metrics need the capability; a protocol-1 SDK's readouts become text metrics.
    let (mut m, task) = connect(&reg, 1);
    m.send(json!({"t":"hello","proto":1,"mod":"old-mod","caps":["metrics"]})).await;
    let w = m.recv().await;
    assert_eq!(w["caps"], json!(["options", "log"]));
    assert!(w.get("limits").is_none());
    m.send(json!({"t":"metrics","defs":[{"id":"old-mod.x","kind":"gauge"}]})).await;
    m.send(json!({"t":"readout","values":{"fg":"on","Bad":"x"}})).await;
    let st = until(|| reg.connections(Some(1)).into_iter().next().filter(|c| !c.metrics.is_empty())).await;
    assert_eq!(st.metrics.len(), 1);
    assert_eq!((st.metrics[0].id.as_str(), st.metrics[0].kind.as_str()), ("old-mod.fg", "text"));
    assert_eq!(st.metrics[0].value, Some(json!("on")));
    assert!(st.refused >= 1);
    drop(m);
    let _ = task.await;
}

#[tokio::test]
async fn rates_are_enforced() {
    let (reg, _rec) = setup();
    let (mut m, task) = connect(&reg, 1);
    m.send(json!({"t":"hello","proto":2,"mod":"m","caps":["metrics"]})).await;
    m.recv().await;
    m.send(json!({"t":"metrics","defs":[{"id":"m.g","kind":"gauge"}]})).await;
    m.recv().await;
    for i in 0..40 {
        m.send(json!({"t":"m","ts":i,"v":{"0":i}})).await;
    }
    let st = until(|| reg.connections(Some(1)).into_iter().next().filter(|c| c.refused > 0)).await;
    // 15 a second get through; the rest are refused.
    assert!(st.refused >= 20, "{st:?}");
    // Declarations: four a second (the first was above), then "retry in a second".
    for _ in 0..3 {
        m.send(json!({"t":"metrics","defs":[{"id":"m.g","kind":"gauge"}]})).await;
        assert!(m.recv().await["rejected"].as_object().is_none_or(|r| r.is_empty()));
    }
    m.send(json!({"t":"metrics","defs":[{"id":"m.g","kind":"gauge"}]})).await;
    assert!(m.recv().await["rejected"]["m.g"].as_str().unwrap().contains("retry"));
    drop(m);
    let _ = task.await;
}

#[test]
fn session_ids() {
    let a = new_session_id();
    let b = new_session_id();
    assert_ne!(a, b);
    assert!(a.starts_with("s-") && a.len() == 12 && wire::is_session_id(&a), "{a}");
}

fn connect_as(reg: &Arc<ModRegistry>, origin: ChannelOrigin) -> (Mod, tokio::task::JoinHandle<Result<()>>) {
    let (to_q, q_in) = duplex(1 << 16);
    let (q_out, from_q) = duplex(1 << 16);
    let reg = reg.clone();
    let task = tokio::spawn(async move { reg.serve(origin, BufReader::new(q_in), q_out).await });
    (Mod { w: to_q, r: BufReader::new(from_q) }, task)
}

/// An Epic game's channel (the mods-only bridge sends `game`): listed under its game, never
/// as Ubisoft product 0; its stereo word said right after hello (before the mod has read the
/// welcome) still lands.
#[tokio::test]
async fn another_stores_channel_is_its_games() {
    let (reg, rec) = setup();
    let awr = quiver_api::GameRef::parse("epic:7315f614b0504081bcdceef138075c9c").unwrap();
    assert_eq!(channel_game(Some("epic:7315f614b0504081bcdceef138075c9c")).unwrap(), Some(awr));
    assert_eq!(channel_game(Some("ubisoft:3539")).unwrap(), None, "a Ubisoft ref is the uplay id's");
    assert!(channel_game(Some("not a ref")).is_err());
    let origin = ChannelOrigin { product_id: 0, game: Some(awr), session: Some("s-epic".into()), bridge_conn: 1, ..Default::default() };
    let (mut m, task) = connect_as(&reg, origin);
    m.send(json!({"t":"hello","proto":3,"mod":"torchbearer","caps":["options"]})).await;
    m.send(json!({"t":"stereo","layout":"hsbs"})).await;
    assert_eq!(m.recv().await["t"], "welcome");
    until(|| rec.take().into_iter().find(|l| l == "stereo Some(\"hsbs\")")).await;
    let c = reg.connections_of(awr);
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].game, c[0].product_id, c[0].stereo.as_deref()), (Some(awr), 0, Some("hsbs")));
    assert!(reg.connections(Some(0)).is_empty(), "not Ubisoft product 0");
    assert_eq!(reg.connections(None).len(), 1);
    m.send(json!({"t":"bye"})).await;
    task.await.unwrap().unwrap();
}

/// quiverd's registry refuses lab sessions; a lab instance's serves only them.
#[tokio::test]
async fn lab_sessions_only_in_a_lab_registry() {
    let (reg, _) = setup();
    let lab_origin = ChannelOrigin { session: Some("lab-awr-r3".into()), bridge_conn: 1, ..Default::default() };
    let (_m, task) = connect_as(&reg, lab_origin.clone());
    assert!(task.await.unwrap().unwrap_err().to_string().contains("lab sessions have no mod channel here"));

    let rec = Arc::new(Rec::default());
    let lab = ModRegistry::lab(|s| s.starts_with("lab-awr-"), rec.clone(), rec.clone());
    let (_m, task) = connect_as(&lab, ChannelOrigin { session: Some("lab-other-r1".into()), ..Default::default() });
    assert!(task.await.unwrap().unwrap_err().to_string().contains("isn't this lab instance's"));
    let (_m, task) = connect_as(&lab, ChannelOrigin { session: Some("s-person".into()), ..Default::default() });
    assert!(task.await.unwrap().unwrap_err().to_string().contains("lab sessions only"));
    let (_m, task) = connect_as(&lab, ChannelOrigin { session: None, ..Default::default() });
    assert!(task.await.unwrap().is_err(), "no session: not a lab one");
    let (mut m, task) = connect_as(&lab, lab_origin);
    m.send(json!({"t":"hello","proto":3,"mod":"torchbearer"})).await;
    assert_eq!(m.recv().await["t"], "welcome");
    m.send(json!({"t":"bye"})).await;
    task.await.unwrap().unwrap();
}
