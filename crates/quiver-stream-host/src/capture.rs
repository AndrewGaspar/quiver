//! Capture from gamescope's PipeWire screencast node into the encoder.
//!
//! gamescope (3.16) offers its output as BGRx or NV12, each as a LINEAR dma-buf or in shared
//! memory. This takes **BGRx in shared memory** (NVENC converts to YUV). Its NV12 in shared
//! memory comes out all zeros, and the CUDA driver can't import a dma-buf directly (only
//! memory Vulkan or GL exported), so the dma-buf path would need a Vulkan import and a copy
//! into exportable memory first. Measured at 2560×1440 (14.7 MB a frame): the copy from the
//! page-locked mapping into CUDA takes about 1.0 ms (p50), and the whole capture → encode start
//! 1.1 ms; gamescope's own path (the program draws → the frame is dequeued here) is about 1.6
//! refreshes, 13 ms at 120 Hz. That, not the copy, is the cost worth removing (see the crate
//! docs on capturing in the Sight layer instead).
//!
//! The PipeWire loop runs on its own thread, with the CUDA context pushed. Each frame is copied
//! from the mapped buffer into one of three CUDA staging buffers and announced to the video
//! thread, which copies the newest into the encoder's input (a device-to-device copy) and
//! encodes it. Frames the encoder hasn't got to are skipped, never queued.
//!
//! The capture time is the buffer's header timestamp when the producer sets one (its
//! `CLOCK_MONOTONIC` at the frame), else the moment it was dequeued. gamescope 3.16 sets none.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{Pod, Property, Value};
use quiver_stream_proto::{Micros, now_micros};

use crate::cuda::{Cuda, DeviceBuf};
use crate::nvenc::{InputFormat, Surface};
use crate::video::{Next, Source};

const SLOTS: usize = 3;

/// One staged frame, in CUDA memory.
struct Slot {
    buf: Option<DeviceBuf>,
    /// Bumped on every write, so a reader can tell the slot was reused under it.
    generation: u64,
    width: u32,
    height: u32,
    format: InputFormat,
}

struct Staging {
    slots: Vec<Mutex<Slot>>,
}

/// A frame is ready in a slot.
#[derive(Debug, Clone, Copy)]
struct Ready {
    slot: usize,
    generation: u64,
    captured: Micros,
    width: u32,
    height: u32,
    format: InputFormat,
    /// When the PipeWire thread got it, and when the copy to the slot was done (µs).
    dequeued: Micros,
    staged: Micros,
}

/// Capture timings, for the report.
#[derive(Debug, Default, Clone)]
pub struct CaptureTimes {
    /// gamescope's frame time to our dequeue, the upload to staging.
    pub dequeue_us: crate::stats::Samples,
    pub upload_us: crate::stats::Samples,
}

/// The capture source for the video thread.
pub struct PwCapture {
    staging: Arc<Staging>,
    rx: std_mpsc::Receiver<Ready>,
    quit: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
    current: Option<(u32, u32, InputFormat)>,
    held: Option<Ready>,
    pub times: Arc<Mutex<CaptureTimes>>,
    failed: Arc<Mutex<Option<String>>>,
}

/// What to ask gamescope for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Nv12,
    Bgrx,
}

impl PwCapture {
    /// Connect to `node` and wait (up to `timeout`) for the first frame's format.
    pub fn start(cuda: Arc<Cuda>, node: u32, want: Want, timeout: Duration) -> Result<PwCapture> {
        let staging = Arc::new(Staging {
            slots: (0..SLOTS).map(|_| Mutex::new(Slot { buf: None, generation: 0, width: 0, height: 0, format: InputFormat::Nv12 })).collect(),
        });
        let (tx, rx) = std_mpsc::channel();
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let times = Arc::new(Mutex::new(CaptureTimes::default()));
        let failed = Arc::new(Mutex::new(None));
        let (st, fl) = (staging.clone(), failed.clone());
        let thread = std::thread::Builder::new().name("pw-video".into()).spawn(move || {
            let r = pw_thread(cuda, node, want, st, tx, quit_rx);
            if let Err(e) = &r {
                *fl.lock().unwrap() = Some(format!("{e:#}"));
            }
            r
        })?;
        let mut c = PwCapture { staging, rx, quit: quit_tx, thread: Some(thread), current: None, held: None, times, failed };
        match c.rx.recv_timeout(timeout) {
            Ok(r) => {
                c.current = Some((r.width, r.height, r.format));
                c.held = Some(r);
                Ok(c)
            }
            Err(_) => {
                let why = c.failed.lock().unwrap().clone().unwrap_or_else(|| "no frame".into());
                c.stop();
                bail!("no frame from PipeWire node {node} within {timeout:?}: {why}")
            }
        }
    }

    pub fn stop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PwCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Source for PwCapture {
    fn format(&self) -> (u32, u32, InputFormat) {
        self.current.or(self.held.map(|r| (r.width, r.height, r.format))).unwrap_or((0, 0, InputFormat::Nv12))
    }

    fn next(&mut self, cuda: &Arc<Cuda>, surface: &mut Surface, _seq: u64, stop: &AtomicBool) -> Result<Next> {
        loop {
            let r = match self.held.take() {
                Some(r) => r,
                None => loop {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(Next::Stopped);
                    }
                    match self.rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(r) => break r,
                        Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                            if let Some(e) = self.failed.lock().unwrap().clone() {
                                bail!("capture: {e}");
                            }
                            return Ok(Next::Stopped);
                        }
                    }
                },
            };
            // Skip to the newest frame.
            let mut r = r;
            while let Ok(n) = self.rx.try_recv() {
                r = n;
            }
            let fmt = (r.width, r.height, r.format);
            if self.current != Some(fmt) || (surface.width, surface.height, surface.format) != fmt {
                self.current = Some(fmt);
                self.held = Some(r);
                return Ok(Next::Changed { width: r.width, height: r.height, format: r.format });
            }
            let slot = self.staging.slots[r.slot].lock().unwrap();
            if slot.generation != r.generation {
                continue;
            }
            let Some(buf) = slot.buf.as_ref() else { continue };
            let row_bytes = r.width as usize * r.format.bytes_per_pixel();
            // SAFETY: the slot and the surface both hold this format at this size.
            unsafe { cuda.copy_dtod(buf.ptr, buf.pitch, surface.row(0), surface.pitch(), row_bytes, r.format.rows(r.height))? };
            drop(slot);
            let mut t = self.times.lock().unwrap();
            t.dequeue_us.push(r.dequeued.saturating_sub(r.captured) as u32);
            t.upload_us.push(r.staged.saturating_sub(r.dequeued) as u32);
            return Ok(Next::Frame { captured: r.captured });
        }
    }
}

fn format_pod(want: Want) -> Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    // Offer only what's wanted: gamescope otherwise picks its own first choice (BGRx).
    let only = match want {
        Want::Nv12 => VideoFormat::NV12,
        Want::Bgrx => VideoFormat::BGRx,
    };
    let mut obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(FormatProperties::VideoFormat, Id, only),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    // NV12 in BT.709, limited range (what the encoder's VUI says).
    if want == Want::Nv12 {
        obj.properties.push(Property::new(spa::sys::SPA_FORMAT_VIDEO_colorMatrix, Value::Id(spa::utils::Id(spa::sys::SPA_VIDEO_COLOR_MATRIX_BT709))));
        obj.properties.push(Property::new(spa::sys::SPA_FORMAT_VIDEO_colorRange, Value::Id(spa::utils::Id(spa::sys::SPA_VIDEO_COLOR_RANGE_16_235))));
    }
    Ok(spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj)).context("serialize the format")?.0.into_inner())
}

/// Buffers in memory we can map, with a header (for the timestamp).
fn buffer_pods() -> Result<Vec<Vec<u8>>> {
    let ser = |o: spa::pod::Object| -> Result<Vec<u8>> { Ok(spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(o)).context("serialize")?.0.into_inner()) };
    let buffers = spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, Value::Int((1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)))],
    };
    let meta = spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_META_type, Value::Id(spa::utils::Id(spa::sys::SPA_META_Header))),
            Property::new(spa::sys::SPA_PARAM_META_size, Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32)),
        ],
    };
    Ok(vec![ser(buffers)?, ser(meta)?])
}

struct PwState {
    cuda: Arc<Cuda>,
    staging: Arc<Staging>,
    tx: std_mpsc::Sender<Ready>,
    format: Option<(u32, u32, InputFormat)>,
    next_slot: usize,
    generation: u64,
    logged_layout: bool,
    /// Mapped buffer memory page-locked for CUDA (address, length).
    pinned: Vec<(usize, usize)>,
}

fn pw_thread(cuda: Arc<Cuda>, node: u32, want: Want, staging: Arc<Staging>, tx: std_mpsc::Sender<Ready>, quit: pw::channel::Receiver<()>) -> Result<()> {
    let _ctx = cuda.push()?;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("PipeWire context")?;
    let core = context.connect_rc(None).context("connect to PipeWire")?;
    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |_| ml.quit());
    let stream = pw::stream::StreamBox::new(
        &core,
        "quiver-stream-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "quiver-stream-capture",
        },
    )
    .context("PipeWire stream")?;
    let state = PwState { cuda: cuda.clone(), staging, tx, format: None, next_slot: 0, generation: 0, logged_layout: false, pinned: Vec::new() };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| tracing::debug!("capture stream: {old:?} -> {new:?}"))
        .param_changed(|stream, st, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let fmt = match info.format() {
                spa::param::video::VideoFormat::NV12 => InputFormat::Nv12,
                spa::param::video::VideoFormat::BGRx | spa::param::video::VideoFormat::BGRA => InputFormat::Bgrx,
                other => {
                    tracing::warn!("capture: unexpected format {other:?}");
                    return;
                }
            };
            let size = info.size();
            tracing::info!("capture: {:?} {}x{} @ {}/{}", info.format(), size.width, size.height, info.framerate().num, info.framerate().denom);
            st.format = Some((size.width, size.height, fmt));
            if let Ok(pods) = buffer_pods() {
                let pods: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
                let mut pods = pods;
                let _ = stream.update_params(&mut pods);
            }
        })
        .remove_buffer(|_, st, buf| {
            // Unpin the buffer's memory before PipeWire unmaps it.
            let Some(spa_buf) = (unsafe { buf.as_ref() }).map(|b| b.buffer) else { return };
            if spa_buf.is_null() {
                return;
            }
            let b = unsafe { &*spa_buf };
            for i in 0..b.n_datas as usize {
                let d = unsafe { &*b.datas.add(i) };
                let addr = d.data as usize;
                if let Some(k) = st.pinned.iter().position(|(a, _)| *a == addr) {
                    if st.pinned[k].1 > 0 {
                        unsafe { st.cuda.host_unregister(addr as *mut u8) };
                    }
                    st.pinned.remove(k);
                }
            }
        })
        .process(|stream, st| {
            let dequeued = now_micros();
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some((w, h, fmt)) = st.format else { return };
            let pts = buffer.find_meta::<spa::buffer::meta::MetaHeader>().map(|m| m.pts()).unwrap_or(0);
            // gamescope's pts is CLOCK_MONOTONIC ns; trust it only when it's recent.
            let captured = if pts > 0 && (dequeued as i64 - pts / 1000).abs() < 200_000 { (pts / 1000) as Micros } else { dequeued };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            if !st.logged_layout {
                st.logged_layout = true;
                let d: Vec<String> = datas.iter().map(|d| format!("type {:?} offset {} size {} stride {}", d.type_(), d.chunk().offset(), d.chunk().size(), d.chunk().stride())).collect();
                tracing::info!("capture: buffer layout {d:?}, header pts {}", if pts > 0 { "yes" } else { "no" });
            }
            if let Err(e) = stage(st, datas, w, h, fmt, captured, dequeued) {
                tracing::warn!("capture: {e:#}");
            }
        })
        .register()?;
    let fmt = format_pod(want)?;
    let mut params = [Pod::from_bytes(&fmt).context("format pod")?];
    stream.connect(spa::utils::Direction::Input, Some(node), pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS, &mut params).context("connect the capture stream")?;
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// Copy the frame in `datas` into the next staging slot and announce it.
fn stage(st: &mut PwState, datas: &mut [spa::buffer::Data], w: u32, h: u32, fmt: InputFormat, captured: Micros, dequeued: Micros) -> Result<()> {
    let row_bytes = w as usize * fmt.bytes_per_pixel();
    let rows = fmt.rows(h);
    let i = st.next_slot;
    st.next_slot = (i + 1) % SLOTS;
    let mut slot = st.staging.slots[i].lock().unwrap();
    if slot.buf.as_ref().is_none_or(|b| b.width_bytes != row_bytes || b.rows != rows) {
        slot.buf = Some(st.cuda.alloc_pitch(row_bytes, rows)?);
    }
    let dst = slot.buf.as_ref().unwrap();
    let (dst_ptr, dst_pitch) = (dst.ptr, dst.pitch);
    // Planes: one data holding them all (offsets from the chunk), or one data per plane.
    let planes: Vec<(usize, usize)> = match fmt {
        InputFormat::Nv12 => vec![(0, h as usize), (h as usize, (h as usize).div_ceil(2))],
        _ => vec![(0, h as usize)],
    };
    for (p, (first_row, n)) in planes.iter().enumerate() {
        // Each plane in its own data, or the second plane after the first in data 0.
        let own = datas.len() > p;
        let data = &mut datas[if own { p } else { 0 }];
        let stride = data.chunk().stride().max(row_bytes as i32) as usize;
        let offset = data.chunk().offset() as usize + if own { 0 } else { h as usize * stride };
        let Some(bytes) = data.data() else { bail!("buffer not mapped") };
        // Page-lock the mapping once, so the copy runs at DMA speed.
        let addr = bytes.as_ptr() as usize;
        if !st.pinned.iter().any(|(a, _)| *a == addr) {
            match unsafe { st.cuda.host_register(bytes.as_mut_ptr(), bytes.len()) } {
                Ok(()) => st.pinned.push((addr, bytes.len())),
                Err(e) => tracing::debug!("capture: can't pin a buffer ({e:#}); copying from pageable memory"),
            }
            if !st.pinned.iter().any(|(a, _)| *a == addr) {
                st.pinned.push((addr, 0));
            }
        }
        let need = offset + stride * (n - 1) + row_bytes;
        if bytes.len() < need {
            bail!("buffer too small: {} < {need}", bytes.len());
        }
        // SAFETY: bounds checked above; the slot has `rows` rows of `row_bytes`.
        unsafe { st.cuda.copy_htod(bytes.as_ptr().add(offset), stride, dst_ptr + (first_row * dst_pitch) as u64, dst_pitch, row_bytes, *n)? };
    }
    st.generation += 1;
    slot.generation = st.generation;
    (slot.width, slot.height, slot.format) = (w, h, fmt);
    let r = Ready { slot: i, generation: st.generation, captured, width: w, height: h, format: fmt, dequeued, staged: now_micros() };
    drop(slot);
    let _ = st.tx.send(r);
    Ok(())
}
