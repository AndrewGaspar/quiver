//! Audio playback: packets into the jitter buffer, out through an SDL audio stream (SDL3's
//! PipeWire backend on this desktop) at a small device buffer.
//!
//! SDL calls [`feed`] on its audio thread whenever the device wants more; each call pulls
//! whole 5 ms frames from the jitter buffer and decodes them there (Opus decode of a 5 ms
//! frame is tens of microseconds), so nothing is decoded ahead of need. The device buffer is
//! asked to be 240 frames (5 ms) with `SDL_HINT_AUDIO_DEVICE_SAMPLE_FRAMES`.

use std::ffi::{CStr, c_int, c_void};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use crossbeam_channel::Receiver;
use quiver_stream_proto::now_micros;
use sdl3_sys::audio::*;
use sdl3_sys::error::SDL_GetError;
use tracing::{info, warn};

use crate::jitter::{JitterBuffer, JitterConfig, Pull};
use crate::link::AudioIn;
use crate::opus;

pub const FRAME: usize = 240; // 5 ms at 48 kHz

pub struct Playback {
    pub buffer: Arc<Mutex<JitterBuffer>>,
    state: *mut State,
    stream: *mut SDL_AudioStream,
    pub device_frames: u32,
}

unsafe impl Send for Playback {}

struct State {
    buffer: Arc<Mutex<JitterBuffer>>,
    decoder: opus::Decoder,
    channels: usize,
    pcm: Vec<f32>,
}

/// SDL's callback: `additional` more bytes are wanted.
unsafe extern "C" fn feed(user: *mut c_void, stream: *mut SDL_AudioStream, additional: c_int, _total: c_int) {
    let st = unsafe { &mut *(user as *mut State) };
    let frame_bytes = (FRAME * st.channels * 4) as c_int;
    let mut left = additional;
    while left > 0 {
        let pull = st.buffer.lock().unwrap().pull();
        let pcm = &mut st.pcm;
        let n = match pull {
            Pull::Packet(p) => st.decoder.decode(Some(&p), false, pcm),
            Pull::Fec(p) => st.decoder.decode(Some(&p), true, pcm),
            Pull::Conceal => st.decoder.decode(None, false, pcm),
            Pull::Silence => {
                pcm.fill(0.0);
                Ok(FRAME)
            }
        };
        if n.is_err() {
            pcm.fill(0.0);
        }
        unsafe { SDL_PutAudioStreamData(stream, pcm.as_ptr() as *const c_void, frame_bytes) };
        left -= frame_bytes;
    }
}

fn sdl_error() -> String {
    unsafe { CStr::from_ptr(SDL_GetError()) }.to_string_lossy().into_owned()
}

impl Playback {
    /// Open the default output for `channels` channels. SDL's audio subsystem must be up.
    pub fn open(channels: u8) -> Result<Self> {
        let decoder = opus::Decoder::new(channels)?;
        let buffer = Arc::new(Mutex::new(JitterBuffer::new(JitterConfig::default())));
        let spec = SDL_AudioSpec { format: SDL_AUDIO_F32, channels: channels as c_int, freq: opus::RATE as c_int };
        let state = Box::into_raw(Box::new(State {
            buffer: buffer.clone(),
            decoder,
            channels: channels as usize,
            pcm: vec![0.0; FRAME * channels as usize],
        }));
        let stream = unsafe { SDL_OpenAudioDeviceStream(SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, &spec, Some(feed), state as *mut c_void) };
        if stream.is_null() {
            unsafe { drop(Box::from_raw(state)) };
            bail!("opening audio: {}", sdl_error());
        }
        let mut dev_spec = SDL_AudioSpec { format: SDL_AUDIO_F32, channels: 0, freq: 0 };
        let mut frames: c_int = 0;
        unsafe {
            let dev = SDL_GetAudioStreamDevice(stream);
            SDL_GetAudioDeviceFormat(dev, &mut dev_spec, &mut frames);
            SDL_ResumeAudioStreamDevice(stream);
        }
        info!("audio: {} ch at {} Hz, device buffer {} frames", channels, dev_spec.freq, frames);
        Ok(Self { buffer, state, stream, device_frames: frames.max(0) as u32 })
    }

    /// The device buffer, in microseconds.
    pub fn device_us(&self) -> u32 {
        (self.device_frames as u64 * 1_000_000 / opus::RATE as u64) as u32
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        unsafe {
            SDL_DestroyAudioStream(self.stream);
            drop(Box::from_raw(self.state));
        }
    }
}

/// Move packets from the link into the jitter buffer until the link closes.
pub fn spawn_receiver(rx: Receiver<AudioIn>, buffer: Arc<Mutex<JitterBuffer>>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("audio-in".into())
        .spawn(move || {
            while let Ok(m) = rx.recv() {
                let mut b = buffer.lock().unwrap();
                match m {
                    AudioIn::Packet(p) => {
                        if !p.mic {
                            b.push(p.seq, p.captured, now_micros(), p.data);
                        }
                    }
                    AudioIn::Lost { first, last } => b.lost(first, last),
                }
            }
        })
        .unwrap_or_else(|e| {
            warn!("audio receiver: {e}");
            std::thread::spawn(|| {})
        })
}
