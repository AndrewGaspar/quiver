//! The stream's audio: its own PipeWire null sink, captured and encoded as Opus.
//!
//! **Nothing reaches the person's speakers.** The stream's program plays into a null sink made
//! for this stream (`quiver-stream-<id>`), which has no output: its sound goes nowhere except
//! to this host's capture of its monitor. The program is pointed at it with `PULSE_SINK`
//! (PulseAudio clients, which is how Wine and Proton play) and `PIPEWIRE_NODE` (PipeWire's
//! ALSA plugin and native clients), see [`Audio::child_env`]. The sink asks for the lowest
//! session priority, so WirePlumber never makes it the default output, and the capture stream
//! never reconnects elsewhere (`node.dont-reconnect`): if the sink went away it would not
//! fall back to the microphone.
//!
//! **Removed when the stream ends, crash included.** The sink is an object this process's
//! PipeWire connection created without `object.linger`, so the PipeWire server destroys it
//! the moment the connection closes, however the process ends.
//!
//! Capture is f32 at 48 kHz in the encoder's channel order (PipeWire remaps by position);
//! every 5 ms of it becomes one [`AudioPacket`], stamped with its first sample's time.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use quiver_stream_proto::{AudioPacket, Micros, now_micros};
use tokio::sync::mpsc;

use crate::opus;

/// The audio side of one stream.
pub struct Audio {
    /// The sink's `node.name`.
    pub sink: String,
    quit: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What to make.
#[derive(Debug, Clone)]
pub struct AudioSpec {
    /// Used in the sink's name.
    pub id: String,
    /// 2, 6 or 8.
    pub channels: u8,
    /// Frame length in tenths of a millisecond (50 = 5 ms).
    pub frame_ms_x10: u32,
    pub bitrate: i32,
    /// Constant bitrate (GameStream's audio FEC needs equal-sized packets).
    pub constant_bitrate: bool,
}

impl AudioSpec {
    pub fn sink_name(&self) -> String {
        format!("quiver-stream-{}", self.id)
    }
}

impl Audio {
    /// Make the sink and start capturing it. Returns once the sink exists, so a program
    /// started after this finds it.
    pub fn start(spec: AudioSpec, packets: mpsc::Sender<AudioPacket>) -> Result<Audio> {
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let sink = spec.sink_name();
        let thread = std::thread::Builder::new().name("pw-audio".into()).spawn(move || {
            if let Err(e) = audio_thread(spec, packets, quit_rx, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        let mut a = Audio { sink, quit: quit_tx, thread: Some(thread) };
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(a),
            Ok(Err(e)) => {
                a.stop();
                Err(e)
            }
            Err(_) => {
                a.stop();
                bail!("the stream's audio sink didn't appear within 10 s")
            }
        }
    }

    /// The environment that points a program at this stream's sink.
    pub fn child_env(&self) -> Vec<(String, String)> {
        vec![("PULSE_SINK".into(), self.sink.clone()), ("PIPEWIRE_NODE".into(), self.sink.clone())]
    }

    pub fn stop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The null sink's properties.
pub fn sink_props(spec: &AudioSpec) -> pw::properties::PropertiesBox {
    pw::properties::properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => spec.sink_name(),
        "node.description" => format!("Quiver stream {}", spec.id),
        "media.class" => "Audio/Sink",
        "audio.channels" => spec.channels.to_string(),
        "audio.position" => opus::sink_positions(spec.channels),
        "audio.rate" => "48000",
        "node.virtual" => "true",
        // Never the default output: real sinks are around 1000 and up.
        "priority.session" => "1",
        "priority.driver" => "1",
        "object.linger" => "false",
    }
}

fn position_id(name: &str) -> u32 {
    match name {
        "MONO" => spa::sys::SPA_AUDIO_CHANNEL_MONO,
        "FL" => spa::sys::SPA_AUDIO_CHANNEL_FL,
        "FR" => spa::sys::SPA_AUDIO_CHANNEL_FR,
        "FC" => spa::sys::SPA_AUDIO_CHANNEL_FC,
        "LFE" => spa::sys::SPA_AUDIO_CHANNEL_LFE,
        "SL" => spa::sys::SPA_AUDIO_CHANNEL_SL,
        "SR" => spa::sys::SPA_AUDIO_CHANNEL_SR,
        "RL" => spa::sys::SPA_AUDIO_CHANNEL_RL,
        "RR" => spa::sys::SPA_AUDIO_CHANNEL_RR,
        _ => 0,
    }
}

/// Samples waiting for a whole frame, and when the oldest was captured.
pub struct Framer {
    channels: usize,
    frame: usize,
    buf: Vec<f32>,
    /// Capture time of `buf[0]`.
    start: Micros,
    seq: u64,
}

impl Framer {
    pub fn new(channels: usize, frame: usize) -> Self {
        Framer { channels, frame, buf: Vec::new(), start: 0, seq: 0 }
    }

    /// Add interleaved samples that ended at `now`; returns the whole frames, each with its
    /// seq and first-sample time.
    pub fn push(&mut self, samples: &[f32], now: Micros) -> Vec<(u64, Micros, Vec<f32>)> {
        let frames_in = samples.len() / self.channels;
        let held = self.buf.len() / self.channels;
        self.start = now.saturating_sub(((held + frames_in) as u64 * 1_000_000) / opus::RATE as u64);
        self.buf.extend_from_slice(&samples[..frames_in * self.channels]);
        let mut out = Vec::new();
        let per = self.frame * self.channels;
        let frame_us = self.frame as u64 * 1_000_000 / opus::RATE as u64;
        while self.buf.len() >= per {
            let rest = self.buf.split_off(per);
            let f = std::mem::replace(&mut self.buf, rest);
            out.push((self.seq, self.start, f));
            self.seq += 1;
            self.start += frame_us;
        }
        out
    }
}

fn audio_thread(spec: AudioSpec, packets: mpsc::Sender<AudioPacket>, quit: pw::channel::Receiver<()>, ready: &std_mpsc::Sender<Result<()>>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("PipeWire context")?;
    let core = context.connect_rc(None).context("connect to PipeWire")?;
    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |_| ml.quit());

    let _sink: pw::node::Node = core.create_object("adapter", &sink_props(&spec)).context("create the stream's null sink")?;
    // Wait for the server to have made it.
    let done = std::rc::Rc::new(std::cell::Cell::new(false));
    let pending = core.sync(0).context("sync")?;
    let (d, ml2) = (done.clone(), mainloop.clone());
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                d.set(true);
                ml2.quit();
            }
        })
        .register();
    mainloop.run();
    if !done.get() {
        return Ok(());
    }

    let channels = spec.channels.clamp(1, 8);
    let mut enc = opus::Encoder::new(channels, spec.frame_ms_x10, spec.bitrate)?;
    enc.set_constant_bitrate(spec.constant_bitrate);
    let channels = enc.channels as usize;
    let mut framer = Framer::new(channels, enc.frame);
    let stream = pw::stream::StreamBox::new(
        &core,
        "quiver-stream-audio",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Game",
            *pw::keys::NODE_NAME => format!("quiver-stream-{}-capture", spec.id),
            "target.object" => spec.sink_name(),
            "stream.capture.sink" => "true",
            "node.dont-reconnect" => "true",
            "node.latency" => format!("{}/48000", enc.frame),
        },
    )
    .context("PipeWire audio stream")?;
    let mut seq_warned = false;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let now = now_micros();
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let d = &mut datas[0];
            let n = d.chunk().size() as usize / 4;
            let off = d.chunk().offset() as usize;
            let Some(bytes) = d.data() else { return };
            let bytes = &bytes[off..(off + n * 4).min(bytes.len())];
            let samples: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            for (seq, captured, pcm) in framer.push(&samples, now) {
                match enc.encode(&pcm) {
                    Ok(data) => {
                        if packets.try_send(AudioPacket { seq, captured, mic: false, data }).is_err() && !seq_warned {
                            seq_warned = true;
                            tracing::warn!("audio: the packet channel is full; dropping packets");
                        }
                    }
                    Err(e) => tracing::warn!("audio: {e:#}"),
                }
            }
        })
        .register()?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(opus::RATE);
    info.set_channels(channels as u32);
    let mut pos = [0u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    for (i, p) in opus::vorbis_positions(channels as u8).iter().enumerate() {
        pos[i] = position_id(p);
    }
    info.set_position(pos);
    let obj = spa::pod::Object { type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(), id: spa::param::ParamType::EnumFormat.as_raw(), properties: info.into() };
    let values = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj)).context("serialize")?.0.into_inner();
    let mut params = [Pod::from_bytes(&values).context("format pod")?];
    stream
        .connect(spa::utils::Direction::Input, None, pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS, &mut params)
        .context("connect the audio capture")?;
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framer_makes_whole_frames_with_first_sample_times() {
        let mut f = Framer::new(2, 240);
        // 100 frames arrive at t=10 ms: nothing yet.
        assert!(f.push(&vec![0.0; 200], 10_000).is_empty());
        // 400 more at t=20 ms: two frames (500 held), the first starting 500 frames (10.4 ms)
        // before 20 ms.
        let out = f.push(&vec![0.0; 800], 20_000);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].0, out[0].1), (0, 20_000 - 10_416));
        assert_eq!((out[1].0, out[1].1), (1, 20_000 - 10_416 + 5_000));
        assert_eq!(out[0].2.len(), 480);
    }

    #[test]
    fn the_sink_is_never_the_default_and_never_lingers() {
        let spec = AudioSpec { id: "abc".into(), channels: 6, frame_ms_x10: 50, bitrate: 256_000, constant_bitrate: false };
        let p = sink_props(&spec);
        assert_eq!(p.get("node.name"), Some("quiver-stream-abc"));
        assert_eq!(p.get("object.linger"), Some("false"));
        assert_eq!(p.get("priority.session"), Some("1"));
        assert_eq!(p.get("audio.position"), Some("FL,FR,FC,LFE,RL,RR"));
        assert_eq!(p.get("factory.name"), Some("support.null-audio-sink"));
    }
}
