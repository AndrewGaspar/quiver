//! Send-side bandwidth estimation, after Google Congestion Control (draft-ietf-rmcat-gcc and
//! libwebrtc's current form of it).
//!
//! The host records every datagram it sends; the client's [`Feedback`] says when each one
//! arrived. From those:
//!
//! - **Delay-based.** Packets are grouped by send time (5 ms groups, with arrival bursts
//!   merged). For each pair of groups, the change in one-way delay is
//!   `Δarrival − Δsend`. The accumulated delay is smoothed and a line is fitted to the
//!   last 20 points (the *trendline*); its slope, scaled, is compared with an adaptive
//!   threshold. A rising slope is overuse: a queue is building. An AIMD controller then
//!   cuts the rate to 0.85 × the rate that actually arrived, and otherwise raises it: by 8 %
//!   a second when far from the last known capacity, by 4 % a second near it.
//! - **Loss-based.** Loss above the path's own baseline (random Wi-Fi loss, which FEC
//!   handles, is not congestion) of more than 10 % cuts the rate by half the loss; under 2 %
//!   excess, the cap lifts.
//! - **Probing.** At the start, short clusters of padding at 3× and 6× the start rate (then
//!   doubling while they get through) measure what the path takes, so the stream reaches
//!   its rate in well under a second instead of ramping for many.
//!
//! The target is the smaller of the two, clamped to the configured range.

use std::collections::VecDeque;

use quiver_stream_proto::Micros;

use crate::feedback::Feedback;

#[derive(Debug, Clone)]
pub struct BweOpts {
    pub min_bps: u64,
    pub max_bps: u64,
    pub start_bps: u64,
    /// Probe at the start (and after a path change).
    pub probe: bool,
}

impl Default for BweOpts {
    fn default() -> Self {
        Self { min_bps: 1_000_000, max_bps: 150_000_000, start_bps: 20_000_000, probe: true }
    }
}

/// What the delay detector concluded from the last group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    Normal,
    Over,
    Under,
}

// --- send history ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Sent {
    at: Micros,
    bytes: u32,
    acked: bool,
    /// When it arrived, on the client's clock, once acked.
    arrival: Micros,
}

/// What was sent, by packet number. Packet numbers are contiguous on the sending side, so
/// this is a deque indexed by `pn - first`.
#[derive(Debug, Default)]
struct SentHistory {
    first: u64,
    sent: VecDeque<Sent>,
}

impl SentHistory {
    const KEEP: Micros = 2_000_000;

    fn push(&mut self, pn: u64, s: Sent) {
        if self.sent.is_empty() {
            self.first = pn;
        }
        // Fill any gap (a packet number skipped by the caller) with an empty record.
        while self.first + self.sent.len() as u64 <= pn {
            let at = s.at;
            self.sent.push_back(Sent { at, bytes: 0, acked: true, arrival: 0 });
        }
        let i = (pn - self.first) as usize;
        self.sent[i] = s;
        while self.sent.front().is_some_and(|f| f.at + Self::KEEP < s.at) {
            self.sent.pop_front();
            self.first += 1;
        }
    }

    fn get_mut(&mut self, pn: u64) -> Option<&mut Sent> {
        let i = pn.checked_sub(self.first)?;
        self.sent.get_mut(i as usize)
    }

    fn range(&self, from: u64, to: u64) -> impl Iterator<Item = (u64, &Sent)> {
        let lo = from.saturating_sub(self.first) as usize;
        let hi = (to + 1).saturating_sub(self.first) as usize;
        self.sent.iter().enumerate().skip(lo).take(hi.saturating_sub(lo)).map(move |(i, s)| (self.first + i as u64, s))
    }
}

// --- grouping ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Group {
    first_send: Micros,
    last_send: Micros,
    first_arrival: Micros,
    last_arrival: Micros,
}

/// Groups packets by send time and yields `(Δsend, Δarrival, arrival)` between groups.
#[derive(Debug, Default)]
struct InterArrival {
    current: Option<Group>,
    prev: Option<Group>,
}

impl InterArrival {
    const GROUP_US: Micros = 5_000;
    const BURST_US: Micros = 5_000;

    fn on_packet(&mut self, send: Micros, arrival: Micros) -> Option<(f64, f64, Micros)> {
        let Some(cur) = &mut self.current else {
            self.current = Some(Group { first_send: send, last_send: send, first_arrival: arrival, last_arrival: arrival });
            return None;
        };
        if send < cur.first_send {
            return None;
        }
        if Self::in_group(cur, send, arrival) {
            cur.last_send = cur.last_send.max(send);
            cur.last_arrival = cur.last_arrival.max(arrival);
            return None;
        }
        let done = *cur;
        let out = self.prev.map(|p| {
            (done.last_send as f64 - p.last_send as f64, done.last_arrival as f64 - p.last_arrival as f64, done.last_arrival)
        });
        self.prev = Some(done);
        self.current = Some(Group { first_send: send, last_send: send, first_arrival: arrival, last_arrival: arrival });
        out.filter(|(_, da, _)| *da >= 0.0)
    }

    fn in_group(g: &Group, send: Micros, arrival: Micros) -> bool {
        if send.saturating_sub(g.first_send) <= Self::GROUP_US {
            return true;
        }
        // An arrival burst (Wi-Fi aggregation, a driver's batch): packets that arrive
        // together, sooner than they were sent apart, belong with the group.
        let send_delta = send as i64 - g.last_send as i64;
        let arrival_delta = arrival as i64 - g.last_arrival as i64;
        arrival_delta - send_delta < 0
            && arrival_delta <= Self::BURST_US as i64
            && arrival.saturating_sub(g.first_arrival) < 100_000
    }
}

// --- trendline and overuse ------------------------------------------------------------------

#[derive(Debug)]
struct Trendline {
    window: VecDeque<(f64, f64)>,
    first_arrival: Option<Micros>,
    accumulated: f64,
    smoothed: f64,
    deltas: u32,
    prev_trend: f64,
    threshold: f64,
    last_threshold_update: Option<Micros>,
    time_over: f64,
    over_count: u32,
    usage: Usage,
}

impl Default for Trendline {
    fn default() -> Self {
        Self {
            window: VecDeque::new(),
            first_arrival: None,
            accumulated: 0.0,
            smoothed: 0.0,
            deltas: 0,
            prev_trend: 0.0,
            threshold: 12.5,
            last_threshold_update: None,
            time_over: -1.0,
            over_count: 0,
            usage: Usage::Normal,
        }
    }
}

impl Trendline {
    const WINDOW: usize = 20;
    const SMOOTHING: f64 = 0.9;
    const GAIN: f64 = 4.0;
    const OVERUSE_MS: f64 = 10.0;

    /// One group delta, in µs. Returns the detector's state after it.
    fn update(&mut self, send_delta: f64, arrival_delta: f64, arrival: Micros) -> Usage {
        let delta_ms = (arrival_delta - send_delta) / 1000.0;
        self.deltas = (self.deltas + 1).min(1000);
        let first = *self.first_arrival.get_or_insert(arrival);
        self.accumulated += delta_ms;
        self.smoothed = Self::SMOOTHING * self.smoothed + (1.0 - Self::SMOOTHING) * self.accumulated;
        self.window.push_back(((arrival - first) as f64 / 1000.0, self.smoothed));
        if self.window.len() > Self::WINDOW {
            self.window.pop_front();
        }
        let trend = if self.window.len() == Self::WINDOW { slope(&self.window).unwrap_or(self.prev_trend) } else { self.prev_trend };
        self.detect(trend, send_delta / 1000.0, arrival);
        self.usage
    }

    fn detect(&mut self, trend: f64, send_delta_ms: f64, now: Micros) {
        if self.deltas < 2 {
            self.usage = Usage::Normal;
            return;
        }
        let modified = f64::from(self.deltas.min(60)) * trend * Self::GAIN;
        if modified > self.threshold {
            if self.time_over < 0.0 {
                self.time_over = send_delta_ms / 2.0;
            } else {
                self.time_over += send_delta_ms;
            }
            self.over_count += 1;
            if self.time_over > Self::OVERUSE_MS && self.over_count > 1 && trend >= self.prev_trend {
                self.time_over = 0.0;
                self.over_count = 0;
                self.usage = Usage::Over;
            }
        } else if modified < -self.threshold {
            self.time_over = -1.0;
            self.over_count = 0;
            self.usage = Usage::Under;
        } else {
            self.time_over = -1.0;
            self.over_count = 0;
            self.usage = Usage::Normal;
        }
        self.prev_trend = trend;
        self.adapt_threshold(modified, now);
    }

    /// The threshold follows |modified trend| (quickly down, slowly up), so a path with
    /// noisy delay doesn't read as permanent overuse, and competing TCP doesn't starve us.
    fn adapt_threshold(&mut self, modified: f64, now: Micros) {
        let last = *self.last_threshold_update.get_or_insert(now);
        if modified.abs() > self.threshold + 15.0 {
            self.last_threshold_update = Some(now);
            return;
        }
        let k = if modified.abs() < self.threshold { 0.039 } else { 0.0087 };
        let dt_ms = ((now.saturating_sub(last)) as f64 / 1000.0).min(100.0);
        self.threshold = (self.threshold + k * (modified.abs() - self.threshold) * dt_ms).clamp(6.0, 600.0);
        self.last_threshold_update = Some(now);
    }
}

/// Least-squares slope of y over x.
fn slope(points: &VecDeque<(f64, f64)>) -> Option<f64> {
    let n = points.len() as f64;
    let (sx, sy) = points.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    let (mx, my) = (sx / n, sy / n);
    let (num, den) = points.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + (x - mx) * (y - my), b + (x - mx) * (x - mx)));
    (den > 0.0).then(|| num / den)
}

// --- acked rate --------------------------------------------------------------------------------

/// The rate at which packets arrived, over a window of arrival time.
#[derive(Debug, Default)]
struct AckedRate {
    window: VecDeque<(Micros, u32)>,
    bytes: u64,
}

impl AckedRate {
    const WINDOW: Micros = 300_000;

    fn on_ack(&mut self, arrival: Micros, bytes: u32) {
        self.window.push_back((arrival, bytes));
        self.bytes += u64::from(bytes);
        let newest = self.window.iter().map(|w| w.0).max().unwrap_or(arrival);
        while self.window.front().is_some_and(|(t, _)| *t + Self::WINDOW < newest) {
            let (_, b) = self.window.pop_front().expect("front exists");
            self.bytes -= u64::from(b);
        }
    }

    /// In bps, once there is at least 100 ms of arrivals.
    fn rate(&self) -> Option<f64> {
        let (lo, hi) = self.window.iter().fold((u64::MAX, 0), |(lo, hi), (t, _)| (lo.min(*t), hi.max(*t)));
        let span = hi.checked_sub(lo)?;
        (span >= 100_000).then(|| self.bytes as f64 * 8e6 / span.max(Self::WINDOW / 2) as f64)
    }
}

// --- AIMD ----------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateState {
    Hold,
    Increase,
    Decrease,
}

/// The link capacity seen at past overuses: an average and its spread.
#[derive(Debug, Default)]
struct LinkCapacity {
    estimate: Option<f64>,
    deviation: f64,
}

impl LinkCapacity {
    fn update(&mut self, sample: f64, alpha: f64) {
        let est = match self.estimate {
            None => sample,
            Some(e) => (1.0 - alpha) * e + alpha * sample,
        };
        let norm = est.max(1.0);
        let err = est - sample;
        self.deviation = ((1.0 - alpha) * self.deviation + alpha * err * err / norm).clamp(0.4e3, 2.5e3);
        self.estimate = Some(est);
    }

    fn upper(&self) -> f64 {
        self.estimate.map_or(f64::INFINITY, |e| e + 3.0 * (self.deviation * e).sqrt())
    }
}

#[derive(Debug)]
struct Aimd {
    rate: f64,
    state: RateState,
    capacity: LinkCapacity,
    last_change: Option<Micros>,
}

impl Aimd {
    const BETA: f64 = 0.85;

    fn new(start: f64) -> Self {
        Self { rate: start, state: RateState::Hold, capacity: LinkCapacity::default(), last_change: None }
    }

    fn update(&mut self, usage: Usage, acked: Option<f64>, now: Micros) {
        self.state = match (usage, self.state) {
            (Usage::Normal, RateState::Hold) => RateState::Increase,
            (Usage::Normal, s) => s,
            (Usage::Over, _) => RateState::Decrease,
            (Usage::Under, _) => RateState::Hold,
        };
        match self.state {
            RateState::Hold => {}
            RateState::Increase => {
                if acked.is_some_and(|a| a > self.capacity.upper()) {
                    self.capacity.estimate = None;
                }
                let dt = self.last_change.map_or(0.0, |t| ((now.saturating_sub(t)) as f64 / 1e6).min(1.0));
                let gain = if self.capacity.estimate.is_some() { 1.04f64 } else { 1.08f64 };
                let increased = self.rate * gain.powf(dt);
                self.rate = self.clamp_to_acked(increased.max(self.rate + 1_000.0 * dt), acked);
                self.last_change = Some(now);
            }
            RateState::Decrease => {
                if let Some(a) = acked {
                    let decreased = Self::BETA * a;
                    if decreased < self.rate {
                        self.rate = decreased;
                    }
                    self.capacity.update(a, 0.05);
                }
                self.state = RateState::Hold;
                self.last_change = Some(now);
            }
        }
    }

    /// The estimate may not run far ahead of what actually got through: the sender may be
    /// application-limited (a still scene).
    fn clamp_to_acked(&self, new: f64, acked: Option<f64>) -> f64 {
        match acked {
            Some(a) => new.min((1.5 * a + 10_000.0).max(self.rate)),
            None => new,
        }
    }
}

// --- loss ---------------------------------------------------------------------------------------

#[derive(Debug)]
struct LossBased {
    last: Option<(u64, u64)>,
    expected: u64,
    received: u64,
    window_start: Option<Micros>,
    /// The loss the path has anyway, which FEC absorbs.
    baseline: f64,
    /// Fast- and slow-moving averages, for FEC planning.
    fast: f64,
    slow: f64,
    cap: Option<f64>,
    last_cut: Option<Micros>,
    last_update: Option<Micros>,
}

impl Default for LossBased {
    fn default() -> Self {
        Self {
            last: None,
            expected: 0,
            received: 0,
            window_start: None,
            baseline: 0.0,
            // Until the first window is measured, plan for a lossy path rather than none.
            fast: 0.02,
            slow: 0.0,
            cap: None,
            last_cut: None,
            last_update: None,
        }
    }
}

impl LossBased {
    const WINDOW: Micros = 100_000;

    /// Takes the report's counters; returns the loss rate when a window closes.
    fn on_counters(&mut self, received: u64, highest: u64, now: Micros) -> Option<f64> {
        let Some((r0, h0)) = self.last else {
            self.last = Some((received, highest));
            self.window_start = Some(now);
            return None;
        };
        if received < r0 || highest < h0 {
            return None;
        }
        self.last = Some((received, highest));
        self.expected += highest - h0;
        self.received += received - r0;
        let start = *self.window_start.get_or_insert(now);
        if now < start + Self::WINDOW || self.expected < 10 {
            return None;
        }
        let loss = (1.0 - self.received as f64 / self.expected as f64).clamp(0.0, 1.0);
        self.expected = 0;
        self.received = 0;
        self.window_start = Some(now);
        Some(loss)
    }

    fn update(&mut self, loss: f64, rate: f64, now: Micros) {
        self.fast = 0.5 * self.fast + 0.5 * loss;
        self.slow = 0.95 * self.slow + 0.05 * loss;
        if loss < self.baseline {
            self.baseline = 0.5 * self.baseline + 0.5 * loss;
        } else {
            self.baseline += 0.02 * (loss - self.baseline);
        }
        let excess = loss - self.baseline;
        let dt = self.last_update.map_or(0.0, |t| (now.saturating_sub(t)) as f64 / 1e6);
        self.last_update = Some(now);
        if loss > 0.10 && excess > 0.05 {
            if self.last_cut.is_none_or(|t| now >= t + 200_000) {
                self.cap = Some(rate.min(self.cap.unwrap_or(f64::INFINITY)) * (1.0 - 0.5 * loss));
                self.last_cut = Some(now);
            }
        } else if excess < 0.02 {
            self.cap = self.cap.map(|c| c * 1.08f64.powf(dt.min(1.0)));
        }
    }

    /// The loss rate to plan FEC for.
    fn planning_loss(&self) -> f64 {
        self.fast.max(self.slow)
    }
}

// --- probing ------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Cluster {
    id: u32,
    rate: f64,
    /// Padding bytes to send in all.
    budget: u64,
    sent: u64,
    next_at: Micros,
    first_pn: Option<u64>,
    last_pn: u64,
    started: Micros,
}

#[derive(Debug, Default)]
struct Prober {
    queue: VecDeque<f64>,
    active: Option<Cluster>,
    /// Clusters sent, waiting for their feedback.
    waiting: Vec<Cluster>,
    next_id: u32,
    pub results: Vec<(f64, f64)>,
}

impl Prober {
    const DURATION: Micros = 20_000;
    const MIN_PACKETS: u64 = 10;

    fn start(&mut self, rates: &[f64]) {
        self.queue = rates.iter().copied().collect();
        self.active = None;
        self.waiting.clear();
    }

    fn busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty() || !self.waiting.is_empty()
    }
}

// --- the estimator -------------------------------------------------------------------------------

/// A snapshot for stats.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BweState {
    pub target_bps: u64,
    pub delay_bps: u64,
    pub loss_cap_bps: Option<u64>,
    pub acked_bps: Option<u64>,
    pub loss: f64,
    pub loss_baseline: f64,
    pub threshold: f64,
    pub overuses: u64,
    pub probes: Vec<(u64, u64)>,
}

#[derive(Debug)]
pub struct Bwe {
    opts: BweOpts,
    sent: SentHistory,
    groups: InterArrival,
    trend: Trendline,
    aimd: Aimd,
    loss: LossBased,
    acked: AckedRate,
    prober: Prober,
    overuses: u64,
    last_loss: f64,
}

impl Bwe {
    pub fn new(opts: BweOpts) -> Self {
        let start = opts.start_bps.clamp(opts.min_bps, opts.max_bps) as f64;
        let mut bwe = Self {
            aimd: Aimd::new(start),
            opts,
            sent: SentHistory::default(),
            groups: InterArrival::default(),
            trend: Trendline::default(),
            loss: LossBased::default(),
            acked: AckedRate::default(),
            prober: Prober::default(),
            overuses: 0,
            last_loss: 0.0,
        };
        bwe.start_probing();
        bwe
    }

    /// Probes from the current rate: 3× and 6×, capped just above the maximum.
    pub fn start_probing(&mut self) {
        if !self.opts.probe {
            return;
        }
        let cap = self.opts.max_bps as f64 * 1.1;
        let r = self.aimd.rate;
        let rates: Vec<f64> = [3.0 * r, 6.0 * r].into_iter().map(|x| x.min(cap)).filter(|x| *x > r * 1.1).collect();
        self.prober.start(&rates);
    }

    /// After a path change: forget what the old path's queue looked like, go back to at most
    /// the start rate, and probe again.
    pub fn reset_path(&mut self) {
        self.groups = InterArrival::default();
        self.trend = Trendline::default();
        self.acked = AckedRate::default();
        let start = self.opts.start_bps.clamp(self.opts.min_bps, self.opts.max_bps) as f64;
        self.aimd = Aimd::new(self.aimd.rate.min(start));
        self.loss.cap = None;
        self.start_probing();
    }

    pub fn set_max(&mut self, max_bps: u64) {
        self.opts.max_bps = max_bps.max(self.opts.min_bps);
    }

    pub fn on_sent(&mut self, pn: u64, at: Micros, bytes: usize, probe: Option<u32>) {
        self.sent.push(pn, Sent { at, bytes: bytes as u32, acked: false, arrival: 0 });
        if let Some(c) = probe.and_then(|id| self.prober.active.as_mut().filter(|c| c.id == id)) {
            c.first_pn.get_or_insert(pn);
            c.last_pn = pn;
        }
    }

    pub fn on_feedback(&mut self, fb: &Feedback, now: Micros) {
        if let Some(loss) = self.loss.on_counters(fb.received, fb.highest, now) {
            self.last_loss = loss;
            self.loss.update(loss, self.target_f(), now);
        }
        let mut usage = None;
        for &(pn, arrival) in &fb.entries {
            let Some(s) = self.sent.get_mut(pn) else { continue };
            if s.acked {
                continue;
            }
            s.acked = true;
            s.arrival = arrival;
            let (send, bytes) = (s.at, s.bytes);
            self.acked.on_ack(arrival, bytes);
            if let Some((ds, da, at)) = self.groups.on_packet(send, arrival) {
                let u = self.trend.update(ds, da, at);
                usage = Some(u);
                self.aimd.update(u, self.acked.rate(), now);
                if u == Usage::Over {
                    self.overuses += 1;
                }
            }
        }
        if usage.is_none() {
            self.aimd.update(self.trend.usage, self.acked.rate(), now);
        }
        self.finish_probes(fb, now);
        self.aimd.rate = self.aimd.rate.clamp(self.opts.min_bps as f64, self.opts.max_bps as f64);
    }

    fn finish_probes(&mut self, fb: &Feedback, now: Micros) {
        let Some(highest) = fb.entries.last().map(|e| e.0) else { return };
        let mut i = 0;
        while i < self.prober.waiting.len() {
            let c = &self.prober.waiting[i];
            let expired = now > c.started + 1_000_000;
            if !expired && c.first_pn.is_some_and(|_| highest <= c.last_pn) {
                i += 1;
                continue;
            }
            let c = self.prober.waiting.remove(i);
            if expired {
                continue;
            }
            let Some(result) = self.probe_result(&c) else { continue };
            self.prober.results.push((c.rate, result));
            if result > self.aimd.rate {
                self.aimd.rate = result.min(self.opts.max_bps as f64);
                self.aimd.capacity.estimate = None;
            }
            // Got (nearly) all through: try twice as high, unless already past the maximum.
            let next = 2.0 * c.rate;
            if result >= 0.9 * c.rate && self.prober.queue.is_empty() && c.rate < self.opts.max_bps as f64 {
                self.prober.queue.push_back(next.min(self.opts.max_bps as f64 * 1.1));
            } else if result < 0.9 * c.rate {
                self.prober.queue.clear();
            }
        }
    }

    /// The rate a finished cluster measured: the lower of how fast its span of packets went
    /// out and how fast it arrived. Media packets sent in the span count too.
    fn probe_result(&self, c: &Cluster) -> Option<f64> {
        let first = c.first_pn?;
        let (mut sent_bytes, mut n, mut got) = (0u64, 0u64, 0u64);
        let (mut s0, mut s1) = (u64::MAX, 0);
        let mut arrivals = Vec::new();
        for (_, s) in self.sent.range(first, c.last_pn) {
            n += 1;
            s0 = s0.min(s.at);
            s1 = s1.max(s.at);
            sent_bytes += u64::from(s.bytes);
            if s.acked {
                got += 1;
                arrivals.push((s.arrival, s.bytes));
            }
        }
        if n < 5 || got < 5 || s1 <= s0 {
            return None;
        }
        // Rates over a span leave out one end's packet: n packets make n - 1 gaps.
        let last_bytes = self.sent.range(c.last_pn, c.last_pn).next().map_or(0, |(_, s)| u64::from(s.bytes));
        let send_rate = (sent_bytes - last_bytes) as f64 * 8e6 / (s1 - s0) as f64;
        arrivals.sort_unstable_by_key(|a| a.0);
        let (a0, a1) = (arrivals[0].0, arrivals[arrivals.len() - 1].0);
        let recv_bytes: u64 = arrivals[1..].iter().map(|a| u64::from(a.1)).sum();
        let recv_rate = if a1 > a0 { recv_bytes as f64 * 8e6 / (a1 - a0) as f64 } else { send_rate };
        // Lost packets didn't get through, whatever the arrival spacing says.
        let recv_rate = recv_rate.min(send_rate * got as f64 / n as f64);
        Some(send_rate.min(recv_rate))
    }

    /// A padding packet of `size` bytes is due now for a probe: returns its cluster id.
    pub fn poll_probe(&mut self, now: Micros, size: usize) -> Option<u32> {
        if self.prober.active.is_none() {
            let rate = self.prober.queue.pop_front()?;
            let id = self.prober.next_id;
            self.prober.next_id += 1;
            let pad_rate = (rate - self.target_f()).max(rate * 0.5);
            let budget = ((pad_rate / 8.0) * (Prober::DURATION as f64 / 1e6)) as u64;
            let budget = budget.max(Prober::MIN_PACKETS * size as u64);
            self.prober.active =
                Some(Cluster { id, rate, budget, sent: 0, next_at: now, first_pn: None, last_pn: 0, started: now });
        }
        let target = self.target_f();
        let c = self.prober.active.as_mut()?;
        if now < c.next_at {
            return None;
        }
        let pad_rate = (c.rate - target).max(c.rate * 0.5);
        c.sent += size as u64;
        c.next_at = c.next_at.max(now.saturating_sub(500)) + (size as f64 * 8e6 / pad_rate) as Micros;
        let id = c.id;
        if c.sent >= c.budget {
            let done = self.prober.active.take().expect("active");
            self.prober.waiting.push(done);
        }
        Some(id)
    }

    /// When the next probe packet is due, if probing.
    pub fn next_probe_at(&self) -> Option<Micros> {
        if let Some(c) = &self.prober.active {
            return Some(c.next_at);
        }
        (!self.prober.queue.is_empty()).then_some(0)
    }

    pub fn probing(&self) -> bool {
        self.prober.busy()
    }

    fn target_f(&self) -> f64 {
        let r = match self.loss.cap {
            Some(c) => self.aimd.rate.min(c),
            None => self.aimd.rate,
        };
        r.clamp(self.opts.min_bps as f64, self.opts.max_bps as f64)
    }

    /// What may be sent in all (media, FEC and headers), in bps.
    pub fn target_bps(&self) -> u64 {
        self.target_f() as u64
    }

    /// The loss rate FEC should plan for.
    pub fn planning_loss(&self) -> f64 {
        self.loss.planning_loss()
    }

    pub fn state(&self) -> BweState {
        BweState {
            target_bps: self.target_bps(),
            delay_bps: self.aimd.rate as u64,
            loss_cap_bps: self.loss.cap.map(|c| c as u64),
            acked_bps: self.acked.rate().map(|r| r as u64),
            loss: self.last_loss,
            loss_baseline: self.loss.baseline,
            threshold: self.trend.threshold,
            overuses: self.overuses,
            probes: self.prober.results.iter().map(|(a, b)| (*a as u64, *b as u64)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends `rate` bps through a bottleneck of `cap` bps for `ms`, with feedback every
    /// 10 ms, packets of 1200 bytes. Returns the estimator.
    fn run(bwe: &mut Bwe, start_pn: &mut u64, t0: Micros, ms: u64, cap: f64, follow: bool) -> Micros {
        let mut queue_free_at = t0;
        let mut now = t0;
        let mut fb = Feedback::default();
        let mut received = 0u64;
        let end = t0 + ms * 1000;
        let mut next_fb = now + 10_000;
        let mut credit = 0.0;
        while now < end {
            let rate = if follow { bwe.target_bps() as f64 } else { 10e6 };
            credit += rate / 8.0 * 0.0001;
            while credit >= 1200.0 {
                credit -= 1200.0;
                let pn = *start_pn;
                *start_pn += 1;
                bwe.on_sent(pn, now, 1200, None);
                let depart = queue_free_at.max(now) + (1200.0 * 8e6 / cap) as Micros;
                queue_free_at = depart;
                received += 1;
                fb.entries.push((pn, depart + 1000));
                fb.highest = pn;
            }
            now += 100;
            if now >= next_fb {
                fb.received = received;
                let ready: Vec<_> = fb.entries.iter().copied().filter(|e| e.1 <= now).collect();
                fb.entries.retain(|e| e.1 > now);
                bwe.on_feedback(&Feedback { received: fb.received, highest: fb.highest, entries: ready }, now);
                next_fb = now + 10_000;
            }
        }
        now
    }

    #[test]
    fn rate_settles_under_a_bottleneck() {
        let mut bwe = Bwe::new(BweOpts { probe: false, start_bps: 5_000_000, ..Default::default() });
        let mut pn = 1;
        let t = run(&mut bwe, &mut pn, 0, 20_000, 30e6, true);
        let r = bwe.target_bps() as f64;
        assert!(r > 0.6 * 30e6 && r < 1.1 * 30e6, "target {r} under a 30 Mbit/s cap");
        // The bottleneck drops: the estimate follows within a second.
        run(&mut bwe, &mut pn, t, 1_000, 10e6, true);
        let r = bwe.target_bps() as f64;
        assert!(r < 1.1 * 10e6, "target {r} a second after the cap fell to 10 Mbit/s");
    }

    #[test]
    fn grouping_reports_queue_growth() {
        let mut ia = InterArrival::default();
        let mut out = Vec::new();
        for i in 0..10u64 {
            // Sent every 6 ms, arriving 1 ms later each time.
            if let Some(d) = ia.on_packet(i * 6000, 10_000 + i * 7000) {
                out.push(d);
            }
        }
        assert!(!out.is_empty());
        for (ds, da, _) in out {
            assert_eq!(da - ds, 1000.0);
        }
    }

    #[test]
    fn steady_random_loss_becomes_the_baseline() {
        let mut l = LossBased::default();
        for i in 0..200 {
            l.update(0.08, 50e6, i * 100_000);
        }
        assert!(l.cap.is_none(), "8% steady loss never cut the rate");
        assert!((l.baseline - 0.08).abs() < 0.01);
        let mut l = LossBased::default();
        l.update(0.3, 50e6, 0);
        assert!(l.cap.unwrap() < 50e6 * 0.9, "a sudden 30% loss cuts");
    }

    #[test]
    fn slope_fits_a_line() {
        let pts: VecDeque<_> = (0..20).map(|i| (i as f64, 3.0 * i as f64 + 1.0)).collect();
        assert!((slope(&pts).unwrap() - 3.0).abs() < 1e-9);
    }
}
