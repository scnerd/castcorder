//! Clock sync and timing metadata in simulation (tier 2): devices whose clocks
//! are offset by up to +-10 s and drift by up to +-200 ppm ping the host over a
//! `SimNetwork` with jitter and asymmetric latency, as on "control". The
//! estimator must recover offset within 1 ms and drift within 5 ppm after 60 s,
//! and a 1 h take's reference points (written, then parsed back) must place
//! every frame on the host timeline within 1 ms. Truth is `SimTime`.
//!
//! Asymmetry is kept at 0.5 ms: NTP-style sync is biased by half of it, which
//! no estimator can see (clocksync module docs).

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use castcorder_core::clocksync::{ClockEstimator, ClockSample, HostTimeline, ping_interval_ms};
use castcorder_core::timing::{self, Header, RefPoint, RefPoints, TimingLine, TimingMap};
use castcorder_port_clock::{Clock, SimClock, SimClockConfig, SimTime};
use castcorder_port_net::{DatagramSocket, LinkConfig, SimNetwork, SimSocket};
use castcorder_protocol::{
    ClientMessage, CoreMessage, Request, RequestId, Response, decode, encode,
};

const HOST_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
/// The host's own clock drift against true time (host time is its clock).
const HOST_PPM: i64 = 37;
const HOST_WALL_START: Duration = SimClockConfig::DEFAULT_WALL_START;
const FINE: Duration = Duration::from_micros(100);

fn us(d: Duration) -> i64 {
    d.as_micros() as i64
}

/// Host time (its timeline) at true time `t`, exactly.
fn host_true_us(t: f64) -> f64 {
    us(HOST_WALL_START) as f64 + t * 1e6 * (1.0 + HOST_PPM as f64 / 1e6)
}

/// Device drift against the host, which the estimator should report.
fn expected_ppm(device_ppm: i64) -> f64 {
    ((1e6 + device_ppm as f64) / (1e6 + HOST_PPM as f64) - 1.0) * 1e6
}

/// Small deterministic PRNG for device-side delivery delays.
struct Rng(u64);
impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Host {
    sock: SimSocket,
    timeline: HostTimeline<SimClock>,
}

impl Host {
    /// Answer every ping that has arrived, as `Call::serve` does.
    fn serve(&self) {
        while let Some((bytes, from)) = self.sock.try_recv_from().unwrap() {
            let t1_us = self.timeline.now_us();
            let ClientMessage::Request { id, request } = decode(&bytes).unwrap();
            let Request::ClockPing { t0_us } = request else {
                panic!("unexpected {request:?}")
            };
            let pong = CoreMessage::Response {
                id,
                response: Response::ClockPong {
                    t0_us,
                    t1_us,
                    t2_us: self.timeline.now_us(),
                    timeline: self.timeline.id(),
                },
            };
            self.sock.send_to(&encode(&pong), from).unwrap();
        }
    }
}

struct Device {
    ppm: i64,
    clock: SimClock,
    sock: SimSocket,
    est: ClockEstimator,
    sent: u32,
    next_ping: Duration,
    /// New samples since the caller last took them.
    fresh: Vec<ClockSample>,
}

impl Device {
    fn new(
        net: &SimNetwork,
        n: u8,
        offset_ms: i64,
        ppm: i64,
        out: Duration,
        back: Duration,
    ) -> Self {
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 1, n));
        let jitter = Duration::from_millis(4);
        net.set_link(
            ip,
            HOST_IP,
            LinkConfig {
                latency: out,
                jitter,
                loss_per_million: 20_000,
            },
        );
        net.set_link(
            HOST_IP,
            ip,
            LinkConfig {
                latency: back,
                jitter,
                loss_per_million: 20_000,
            },
        );
        let wall = if offset_ms >= 0 {
            HOST_WALL_START + Duration::from_millis(offset_ms as u64)
        } else {
            HOST_WALL_START - Duration::from_millis(offset_ms.unsigned_abs())
        };
        Self {
            ppm,
            clock: net.time().clock(SimClockConfig {
                monotonic_at_start: Duration::from_secs(u64::from(n) * 1000),
                wall_at_start: wall,
                drift_ppm: ppm,
            }),
            sock: net.bind(ip),
            est: ClockEstimator::default(),
            sent: 0,
            next_ping: Duration::ZERO,
            fresh: Vec::new(),
        }
    }

    fn now_us(&self) -> i64 {
        us(self.clock.wall())
    }

    /// Ping if due; take pongs. Returns the next ping's true time.
    fn step(&mut self, now: Duration, host: SocketAddrOf) -> Duration {
        while let Some((bytes, _)) = self.sock.try_recv_from().unwrap() {
            let t3_us = self.now_us();
            let CoreMessage::Response {
                response:
                    Response::ClockPong {
                        t0_us,
                        t1_us,
                        t2_us,
                        ..
                    },
                ..
            } = decode(&bytes).unwrap()
            else {
                panic!("unexpected message");
            };
            let s = ClockSample {
                t0_us,
                t1_us,
                t2_us,
                t3_us,
            };
            if self.est.push(s) {
                self.fresh.push(s);
            }
        }
        if now >= self.next_ping {
            let ping = ClientMessage::Request {
                id: RequestId(u64::from(self.sent)),
                request: Request::ClockPing {
                    t0_us: self.now_us(),
                },
            };
            self.sock.send_to(&encode(&ping), host.0).unwrap();
            self.next_ping = now + Duration::from_millis(u64::from(ping_interval_ms(self.sent)));
            self.sent += 1;
        }
        self.next_ping
    }

    /// Host-time error of the current estimate, in microseconds.
    fn offset_error_us(&self, now: Duration) -> f64 {
        self.est.host_us(self.now_us()).unwrap() - host_true_us(now.as_secs_f64())
    }
}

#[derive(Clone, Copy)]
struct SocketAddrOf(std::net::SocketAddr);

fn setup(seed: u64) -> (SimTime, SimNetwork, Host) {
    let time = SimTime::new();
    let net = SimNetwork::new(time.clone(), seed);
    let host_clock = time.clock(SimClockConfig {
        monotonic_at_start: Duration::from_secs(77),
        wall_at_start: HOST_WALL_START,
        drift_ppm: HOST_PPM,
    });
    let host = Host {
        sock: net.bind(HOST_IP),
        timeline: HostTimeline::new(host_clock),
    };
    (time, net, host)
}

/// Advance to `target`, finely while datagrams are in flight.
fn advance(time: &SimTime, net: &SimNetwork, target: Duration) {
    let now = time.now();
    let to = if net.in_flight() > 0 {
        target.min(now + FINE)
    } else {
        target
    };
    time.advance(to.saturating_sub(now).max(Duration::from_micros(1)));
}

#[test]
fn estimator_recovers_offset_and_drift_within_60_s() {
    let configs: [(i64, i64, u64, u64); 5] = [
        (10_000, 200, 10_000, 10_500),
        (-10_000, -200, 25_000, 24_500),
        (3_333, -150, 3_000, 3_500),
        (-7_250, 80, 40_000, 40_500),
        (0, 0, 8_000, 8_000),
    ];
    let (mut worst_offset, mut worst_drift) = (0f64, 0f64);
    for seed in 1..=4u64 {
        let (time, net, host) = setup(seed);
        let host_addr = SocketAddrOf(host.sock.local_addr());
        let mut devices: Vec<Device> = configs
            .iter()
            .enumerate()
            .map(|(i, &(off, ppm, out, back))| {
                Device::new(
                    &net,
                    i as u8 + 1,
                    off,
                    ppm,
                    Duration::from_micros(out),
                    Duration::from_micros(back),
                )
            })
            .collect();
        let end = Duration::from_secs(60);
        while time.now() < end {
            host.serve();
            let mut next = end;
            for d in &mut devices {
                next = next.min(d.step(time.now(), host_addr));
            }
            advance(&time, &net, next);
        }
        for d in &devices {
            let est = d.est.estimate().unwrap();
            let off = d.offset_error_us(time.now());
            let drift = est.drift_ppm - expected_ppm(d.ppm);
            println!(
                "seed {seed} device ppm {:+4}: offset error {off:+8.1} us, drift error {drift:+6.2} ppm, rtt {} us, {} points",
                d.ppm, est.rtt_us, est.points
            );
            worst_offset = worst_offset.max(off.abs());
            worst_drift = worst_drift.max(drift.abs());
        }
    }
    println!(
        "worst: offset {worst_offset:.1} us (limit 1000), drift {worst_drift:.2} ppm (limit 5)"
    );
    assert!(worst_offset < 1000.0, "offset error {worst_offset} us");
    assert!(worst_drift < 5.0, "drift error {worst_drift} ppm");
}

#[test]
fn reference_points_place_a_one_hour_take_within_1_ms() {
    const RATE: u32 = 48_000;
    const BATCH: u64 = 960;
    // The audio clock runs 30 ppm off the device clock (device -120 ppm).
    const DEVICE_PPM: i64 = -120;
    const AUDIO_PPM: f64 = -90.0;
    let (time, net, host) = setup(9);
    let host_addr = SocketAddrOf(host.sock.local_addr());
    let mut dev = Device::new(
        &net,
        1,
        -9_500,
        DEVICE_PPM,
        Duration::from_millis(15),
        Duration::from_micros(15_500),
    );
    let mut rng = Rng(42);
    // Sync for 5 s before the take, as a guest does between join and a take.
    let take_start = Duration::from_secs(5);
    let take_len = 3600.0;
    let audio_rate = f64::from(RATE) * (1.0 + AUDIO_PPM / 1e6);
    // True capture time of frame f.
    let captured = |f: u64| take_start.as_secs_f64() + f as f64 / audio_rate;
    let mut text = String::new();
    let mut refs = RefPoints::new(RATE, u64::from(RATE));
    let mut batch = 1u64;
    let mut next_audio = Duration::from_secs_f64(captured(BATCH) + 0.003 * rng.unit());
    let end = Duration::from_secs_f64(captured(0) + take_len);
    let mut started = false;
    while time.now() < end {
        host.serve();
        let next_ping = dev.step(time.now(), host_addr);
        if time.now() >= take_start && !started {
            started = true;
            text += &TimingLine::Header(Header {
                version: timing::VERSION,
                call: "sim".into(),
                device: "dev".into(),
                take: 1,
                track: "take0001-0-mic".into(),
                source: "mic".into(),
                sample_rate: RATE,
                channels: 1,
                device_clock: "SimClock wall".into(),
                host_clock: "HostTimeline".into(),
                host_timeline: host.timeline.id(),
            })
            .to_line();
            text += &TimingLine::Segment {
                segment: 1,
                file: "take0001-0-mic.wav".into(),
            }
            .to_line();
            for s in dev.est.samples() {
                text += &TimingLine::Sync(*s).to_line();
            }
            dev.fresh.clear();
        }
        if started {
            for s in dev.fresh.drain(..) {
                text += &TimingLine::Sync(s).to_line();
            }
        }
        if time.now() >= next_audio {
            let frame = batch * BATCH;
            if let Some((f, d)) = refs.observe(frame, dev.now_us() as f64) {
                text +=
                    &TimingLine::Ref(RefPoint::new(f, d, dev.est.estimate().as_ref())).to_line();
            }
            batch += 1;
            next_audio = Duration::from_secs_f64(captured(batch * BATCH) + 0.003 * rng.unit());
        }
        advance(&time, &net, next_ping.min(next_audio).min(end));
    }
    if let Some((f, d)) = refs.finish() {
        text += &TimingLine::Ref(RefPoint::new(f, d, dev.est.estimate().as_ref())).to_line();
    }
    text += &TimingLine::End {
        frames: batch * BATCH,
    }
    .to_line();

    let map = TimingMap::from_lines(&timing::parse(&text).unwrap());
    assert_eq!(map.header.as_ref().unwrap().sample_rate, RATE);
    assert!(map.sync.len() > 3000, "{} sync lines", map.sync.len());
    let refs = &map.segments[0].refs;
    assert!(refs.len() >= 3599, "{} refs", refs.len());
    let mut worst = 0f64;
    let last = (batch - 1) * BATCH;
    for i in 0..=1000u64 {
        let f = last * i / 1000 + i % 7;
        let got = map.host_us(1, f).unwrap();
        let err = got - host_true_us(captured(f));
        worst = worst.max(err.abs());
    }
    println!(
        "1 h take: {} refs, {} sync lines, {} bytes; worst frame placement error {worst:.1} us",
        refs.len(),
        map.sync.len(),
        text.len()
    );
    assert!(worst < 1000.0, "frame placement error {worst} us");
}
