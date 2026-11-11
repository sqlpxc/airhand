//! 声音：采集被控端正在播放的声音（Windows：WASAPI 环回；Linux：PulseAudio/PipeWire 监视器；
//! macOS：ScreenCaptureKit），Opus 编码后发给控制端。
//!
//! * 所有会话共享一个采集 + 编码线程（引用计数，最后一个订阅者离开时停止）；
//! * 没有声音时不发包（静音检测 + 拖尾），静止的桌面不会因为声音占用带宽；
//! * 每个会话一个有界队列，网络拥塞时直接丢包，不会让声音越积越慢；
//! * 默认输出设备变化（例如插上耳机）或设备失效时自动重新打开；打不开时把原因告诉订阅者
//!   （会话据此给控制端发 `Notice::AudioFailed` / `AudioRecovered`）。

#[cfg(target_os = "linux")]
pub(crate) mod pulse;
#[cfg(target_os = "macos")]
pub(crate) mod sck;
#[cfg(windows)]
pub(crate) mod wasapi;

#[cfg(target_os = "linux")]
use pulse as backend;
#[cfg(target_os = "macos")]
use sck as backend;
#[cfg(windows)]
use wasapi as backend;

/// 测试用：在本机默认输出设备上播放测试音
#[cfg(all(test, any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) use backend::TonePlayer;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use airhand_proto::media::{self, AUDIO_CHANNELS, AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE, AudioHeader};
use anyhow::Result;
use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// 每帧每声道的采样数（20 ms = 960）
pub const FRAME_SAMPLES: usize = (AUDIO_SAMPLE_RATE / 1000 * AUDIO_FRAME_MS) as usize;
const CHANNELS: usize = AUDIO_CHANNELS as usize;
/// 立体声 96 kbps：音乐也基本听不出损失，只占 1080p 视频码率的零头
const BITRATE: i32 = 96_000;
/// 峰值低于 -90 dBFS（16 位 PCM 的 1 LSB）视为静音。环回采集到的是系统音量调节之后的声音，
/// 音量调得很低时有效声音也可能只有 -70 dBFS 左右，所以阈值只排除数字静音/抖动噪声
const SILENCE_PEAK: f32 = 1.0 / 32768.0;
/// 声音停下后再多发 300 ms，说话的短暂停顿不会让控制端反复重新缓冲
const HANGOVER_FRAMES: u32 = 15;
/// 采集端这么久没有数据（没有任何程序在播放）就把缓冲里剩下的半帧补零发出去
const IDLE_FLUSH: Duration = Duration::from_millis(60);
/// 每个会话的发送队列（约 320 ms），满了就丢
pub const QUEUE: usize = 16;

// ───────────────────────────── 编码 ─────────────────────────────

/// 把 48 kHz 立体声交错 PCM 切成 20 ms 一帧编码为 Opus，并去掉静音段。
pub struct Packetizer {
    enc: opus::Encoder,
    /// 编码器前瞻：解码输出比输入晚这么久
    lookahead: Duration,
    pcm: Vec<f32>,
    /// `pcm[0]` 的采集时间
    t0: Option<Instant>,
    frame: Vec<f32>,
    out: Vec<u8>,
    /// 连续静音帧数
    quiet: u32,
    /// 当前是否在发包
    active: bool,
}

/// 每产生一个 Opus 包调用一次：`(包, 是否为静音后的第一包, 解码后第一个采样对应的采集时间)`
pub type Emit<'a> = dyn FnMut(&[u8], bool, Instant) + 'a;

fn frames_dur(frames: usize) -> Duration {
    Duration::from_secs_f64(frames as f64 / AUDIO_SAMPLE_RATE as f64)
}

impl Packetizer {
    pub fn new() -> Result<Self> {
        let mut enc = opus::Encoder::new(
            AUDIO_SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )?;
        enc.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        enc.set_complexity(8)?;
        let lookahead = frames_dur(enc.get_lookahead()?.max(0) as usize);
        Ok(Self {
            enc,
            lookahead,
            pcm: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 4),
            t0: None,
            frame: vec![0.0; FRAME_SAMPLES * CHANNELS],
            // 编码器保证每包不超过这么大：加密之后正好放得进一个数据报（见 airhand_proto::media）
            out: vec![0; media::MAX_OPUS_PACKET],
            quiet: 0,
            active: false,
        })
    }

    /// 追加一段 PCM，`captured` 为这段数据第一个采样的采集时间（没有时按"刚刚采集完"估算）。
    pub fn push(&mut self, pcm: &[f32], captured: Option<Instant>, emit: &mut Emit) -> Result<()> {
        let held = self.pcm.len() / CHANNELS;
        match captured {
            // 以最新的时间戳为准重新对齐，误差不会累积
            Some(t) => self.t0 = Some(t.checked_sub(frames_dur(held)).unwrap_or(t)),
            None if self.t0.is_none() => {
                let now = Instant::now();
                self.t0 = Some(
                    now.checked_sub(frames_dur(held + pcm.len() / CHANNELS))
                        .unwrap_or(now),
                );
            }
            None => {}
        }
        self.pcm.extend_from_slice(pcm);
        let t0 = self.t0.unwrap_or_else(Instant::now);
        let n = FRAME_SAMPLES * CHANNELS;
        let mut off = 0;
        let mut result = Ok(());
        while self.pcm.len() - off >= n {
            self.frame.copy_from_slice(&self.pcm[off..off + n]);
            let t = t0 + frames_dur(off / CHANNELS);
            off += n;
            if let Err(e) = self.encode_frame(t, emit) {
                result = Err(e);
                break;
            }
        }
        self.pcm.drain(..off);
        self.t0 = Some(t0 + frames_dur(off / CHANNELS));
        result
    }

    /// 采集端暂时没有数据：把剩下的半帧补零发出去，再补一帧静音把编码器前瞻里的尾音送出，然后停止发包。
    pub fn flush(&mut self, emit: &mut Emit) -> Result<()> {
        let mut t = self.t0.take().unwrap_or_else(Instant::now);
        if !self.pcm.is_empty() {
            self.frame.fill(0.0);
            self.frame[..self.pcm.len()].copy_from_slice(&self.pcm);
            self.pcm.clear();
            self.encode_frame(t, emit)?;
            t += frames_dur(FRAME_SAMPLES);
        }
        if self.active {
            self.frame.fill(0.0);
            self.encode_frame(t, emit)?;
        }
        self.active = false;
        self.quiet = 0;
        Ok(())
    }

    /// `captured`：这一帧第一个输入采样的采集时间
    fn encode_frame(&mut self, captured: Instant, emit: &mut Emit) -> Result<()> {
        let loud = self.frame.iter().any(|s| s.abs() > SILENCE_PEAK);
        self.quiet = if loud {
            0
        } else {
            self.quiet.saturating_add(1)
        };
        let mut start = false;
        if !self.active {
            if !loud {
                return Ok(());
            }
            self.active = true;
            start = true;
        } else if self.quiet > HANGOVER_FRAMES {
            self.active = false;
            return Ok(());
        }
        let len = self.enc.encode_float(&self.frame, &mut self.out)?;
        // 解码输出比输入晚一个前瞻：解码后第一个采样对应的是更早的输入
        emit(
            &self.out[..len],
            start,
            captured.checked_sub(self.lookahead).unwrap_or(captured),
        );
        Ok(())
    }
}

// ───────────────────────────── 订阅 ─────────────────────────────

struct Subscriber {
    id: u64,
    tx: mpsc::Sender<Bytes>,
    /// 还没有收到过包：第一包要标记为 start
    fresh: bool,
}

struct Shared {
    subs: Mutex<Vec<Subscriber>>,
    stop: AtomicBool,
    /// 打不开声音采集的原因（英文技术信息，见 `capture::wire_detail`）。正常时为 `None`；
    /// 缺少系统权限时也是 `None`：会话已经用 `Notice::MissingPermissions` 说明了
    failure: watch::Sender<Option<String>>,
}

static HUB: LazyLock<Mutex<Weak<Shared>>> = LazyLock::new(|| Mutex::new(Weak::new()));
static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// 声音订阅，Drop 时自动退订。
pub struct AudioSubscription {
    shared: Arc<Shared>,
    id: u64,
    failure: watch::Receiver<Option<String>>,
}

impl AudioSubscription {
    fn new(shared: Arc<Shared>, id: u64) -> Self {
        let failure = shared.failure.subscribe();
        Self {
            shared,
            id,
            failure,
        }
    }

    /// 现在为什么采集不到声音（`None` = 正常）
    pub fn failure(&self) -> Option<String> {
        self.failure.borrow().clone()
    }

    /// 等到采集状态有变化，返回新的状态（取消安全）
    pub async fn failure_changed(&mut self) -> Option<String> {
        // 发送端在 Shared 里，订阅存在期间不会被释放，所以不会出错
        if self.failure.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        self.failure.borrow_and_update().clone()
    }
}

impl Drop for AudioSubscription {
    fn drop(&mut self) {
        // 先拿 hub 锁，保证与 subscribe 的"复用判断"互斥
        let mut hub = HUB.lock().unwrap();
        let mut subs = self.shared.subs.lock().unwrap();
        subs.retain(|s| s.id != self.id);
        if subs.is_empty() {
            self.shared.stop.store(true, Ordering::Release);
            if std::ptr::eq(hub.as_ptr(), Arc::as_ptr(&self.shared)) {
                *hub = Weak::new();
            }
        }
    }
}

/// 订阅被控端声音：声音包（[`media::frame`] 编好的，带长度前缀）会发到 `tx`。
pub fn subscribe(tx: mpsc::Sender<Bytes>) -> AudioSubscription {
    let mut hub = HUB.lock().unwrap();
    let id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
    let sub = Subscriber {
        id,
        tx,
        fresh: true,
    };
    if let Some(shared) = hub.upgrade()
        && !shared.stop.load(Ordering::Acquire)
    {
        shared.subs.lock().unwrap().push(sub);
        return AudioSubscription::new(shared, id);
    }
    let shared = Arc::new(Shared {
        subs: Mutex::new(vec![sub]),
        stop: AtomicBool::new(false),
        failure: watch::Sender::new(None),
    });
    *hub = Arc::downgrade(&shared);
    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name("airhand-audio".into())
        .spawn(move || capture_loop(thread_shared))
        .expect("spawn audio thread");
    AudioSubscription::new(shared, id)
}

/// 打不开声音采集时要告诉控制端的原因；缺少系统权限时为 `None`（另有提示）。
fn failure_detail(e: &anyhow::Error) -> Option<String> {
    (!super::missing_permission(e)).then(|| super::capture::wire_detail(e))
}

/// 更新采集状态，有变化才通知订阅者（每次重试失败的原因通常都一样）。
fn set_failure(shared: &Shared, failure: Option<String>) {
    shared.failure.send_if_modified(|f| {
        let changed = *f != failure;
        *f = failure;
        changed
    });
}

/// 把一个 Opus 包发给所有订阅者（每种 start 取值只打包一次）。
fn broadcast(shared: &Shared, seq: u32, data: &[u8], start: bool, captured: Instant) {
    let capture_us = super::clock::us(captured);
    let encode = |start: bool| {
        let header = AudioHeader {
            seq,
            start,
            capture_us,
        };
        Bytes::from(media::frame(&header.encode(), &[data]))
    };
    let (mut normal, mut first) = (None, None);
    let mut subs = shared.subs.lock().unwrap();
    for s in subs.iter_mut() {
        let msg = if start || s.fresh {
            first.get_or_insert_with(|| encode(true)).clone()
        } else {
            normal.get_or_insert_with(|| encode(false)).clone()
        };
        s.fresh = false;
        // 队列满（网络跟不上）就丢掉这一包
        if let Err(mpsc::error::TrySendError::Full(_)) = s.tx.try_send(msg) {
            debug!(sub = s.id, "audio queue full, dropping a packet");
        }
    }
}

// ───────────────────────────── 采集线程 ─────────────────────────────

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn capture_loop(_shared: Arc<Shared>) {
    info!("audio capture isn't supported on this platform");
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn capture_loop(shared: Arc<Shared>) {
    super::capture::raise_priority();
    let mut packetizer = match Packetizer::new() {
        Ok(p) => p,
        Err(e) => {
            warn!("can't create the Opus encoder: {e:#}");
            set_failure(
                &shared,
                failure_detail(&e.context("can't create the Opus encoder")),
            );
            return;
        }
    };
    let mut seq = 0u32;
    let mut emit = |data: &[u8], start: bool, captured: Instant| {
        seq = seq.wrapping_add(1);
        broadcast(&shared, seq, data, start, captured);
    };
    let mut cap: Option<backend::Loopback> = None;
    let mut failures = 0u32;
    let mut pcm = Vec::with_capacity(AUDIO_SAMPLE_RATE as usize / 10 * CHANNELS);
    let mut last_data = Instant::now();
    let mut flushed = true;
    let mut last_check = Instant::now();
    info!("audio capture started");

    while !shared.stop.load(Ordering::Acquire) {
        let c = match cap.as_mut() {
            Some(c) => c,
            None => match backend::Loopback::open() {
                Ok(c) => {
                    failures = 0;
                    last_check = Instant::now();
                    set_failure(&shared, None);
                    cap.insert(c)
                }
                Err(e) => {
                    failures += 1;
                    set_failure(&shared, failure_detail(&e));
                    if failures == 1 || failures.is_multiple_of(30) {
                        if super::missing_permission(&e) {
                            warn!("can't open audio capture: {e:#}");
                        } else {
                            warn!("can't open audio capture (no audio output device?): {e:#}");
                        }
                    }
                    // 设备可能稍后出现（例如插上耳机），定期重试
                    for _ in 0..20 {
                        if shared.stop.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    continue;
                }
            },
        };

        pcm.clear();
        let (mut lost, captured) = match c.read(&mut pcm) {
            Ok(t) => (false, t),
            Err(e) => {
                info!("audio capture interrupted, reopening: {e:#}");
                (true, None)
            }
        };
        if !lost && last_check.elapsed() > Duration::from_secs(1) {
            last_check = Instant::now();
            if c.default_changed() {
                info!("default audio output device changed, switching capture device");
                lost = true;
            }
        }

        if !pcm.is_empty() {
            last_data = Instant::now();
            flushed = false;
            if let Err(e) = packetizer.push(&pcm, captured, &mut emit) {
                warn!("Opus encoding failed: {e:#}");
            }
        }
        if lost || (!flushed && last_data.elapsed() > IDLE_FLUSH) {
            flushed = true;
            if let Err(e) = packetizer.flush(&mut emit) {
                warn!("Opus encoding failed: {e:#}");
            }
        }
        if lost {
            cap = None;
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    info!("audio capture stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, amp: f32, phase: &mut f64) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let s = (*phase).sin() as f32 * amp;
            *phase += 2.0 * std::f64::consts::PI * 440.0 / AUDIO_SAMPLE_RATE as f64;
            v.extend_from_slice(&[s, s]);
        }
        v
    }

    type Packets = std::cell::RefCell<Vec<(Vec<u8>, bool, Instant)>>;

    /// 模拟采集：按 10 ms 一块喂入，每块带上按采样数推算的采集时间
    struct Feeder {
        base: Instant,
        frames: usize,
    }

    impl Feeder {
        fn push(&mut self, p: &mut Packetizer, pcm: &[f32], emit: &mut Emit) {
            for chunk in pcm.chunks(480 * 2) {
                p.push(chunk, Some(self.base + frames_dur(self.frames)), emit)
                    .unwrap();
                self.frames += chunk.len() / 2;
            }
        }
        fn at(&self) -> Instant {
            self.base + frames_dur(self.frames)
        }
    }

    fn ms(a: Instant, b: Instant) -> f64 {
        (a.max(b) - a.min(b)).as_secs_f64() * 1000.0 * if a >= b { 1.0 } else { -1.0 }
    }

    #[test]
    fn packetizer_gates_silence_and_roundtrips() {
        let mut p = Packetizer::new().unwrap();
        let packets = Packets::default();
        let mut emit =
            |d: &[u8], s: bool, t: Instant| packets.borrow_mut().push((d.to_vec(), s, t));
        let len = || packets.borrow().len();
        let mut f = Feeder {
            base: Instant::now(),
            frames: 0,
        };

        // 纯静音：不发包
        f.push(&mut p, &vec![0.0; FRAME_SAMPLES * 2 * 10], &mut emit);
        assert_eq!(len(), 0, "静音不应发包");

        // 1 秒声音
        let tone_at = f.at();
        let mut phase = 0.0;
        f.push(
            &mut p,
            &tone(AUDIO_SAMPLE_RATE as usize, 0.3, &mut phase),
            &mut emit,
        );
        assert_eq!(len(), 50);
        assert!(packets.borrow()[0].1, "第一包应标记 start");
        assert!(packets.borrow()[1..].iter().all(|(_, s, _)| !s));
        // 时间戳：第一包 = 声音开始 - 编码器前瞻，之后每包 +20 ms
        {
            let pk = packets.borrow();
            let d = ms(pk[0].2, tone_at) + p.lookahead.as_secs_f64() * 1000.0;
            assert!(d.abs() < 0.01, "第一包时间戳偏差 {d} ms");
            for w in pk.windows(2) {
                assert!((ms(w[1].2, w[0].2) - 20.0).abs() < 0.01);
            }
        }

        // 声音停止：拖尾之后不再发包
        let before = len();
        f.push(&mut p, &vec![0.0; FRAME_SAMPLES * 2 * 40], &mut emit);
        assert_eq!(len() - before, HANGOVER_FRAMES as usize);

        // 再次出声：新的 start，时间戳接着采集时间走
        let again = f.at();
        f.push(&mut p, &tone(FRAME_SAMPLES, 0.3, &mut phase), &mut emit);
        {
            let pk = packets.borrow();
            let last = pk.last().unwrap();
            assert!(last.1);
            assert!((ms(last.2, again) + p.lookahead.as_secs_f64() * 1000.0).abs() < 0.01);
        }

        // 半帧 + 采集端停止供数：flush 补齐并多送一帧
        let before = len();
        f.push(&mut p, &tone(300, 0.3, &mut phase), &mut emit);
        p.flush(&mut emit).unwrap();
        assert_eq!(len() - before, 2);
        p.flush(&mut emit).unwrap();
        assert_eq!(len() - before, 2, "重复 flush 不应再发包");

        // 解码验证：声音段的能量应接近原始信号（0.3 正弦波 RMS ≈ 0.212）
        let mut dec = opus::Decoder::new(AUDIO_SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES * 2];
        let mut sum = 0.0f64;
        let mut count = 0usize;
        let packets = packets.take();
        for (i, (pkt, _, _)) in packets.iter().enumerate().take(50) {
            let n = dec.decode_float(pkt, &mut out, false).unwrap();
            assert_eq!(n, FRAME_SAMPLES);
            if i >= 5 {
                sum += out.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
                count += out.len();
            }
        }
        let rms = (sum / count as f64).sqrt();
        assert!((rms - 0.212).abs() < 0.03, "解码后 RMS 异常: {rms}");
        let bytes: usize = packets.iter().take(50).map(|(p, ..)| p.len()).sum();
        println!(
            "1 秒声音 {bytes} 字节（{:.0} kbps），RMS {rms:.3}",
            bytes as f64 * 8.0 / 1000.0
        );
    }

    /// 音画同步的基础：解码后声音出现的位置 + 包时间戳 = 声音真正被采集的时间
    #[test]
    fn decoded_onset_matches_capture_time() {
        let mut p = Packetizer::new().unwrap();
        let packets = Packets::default();
        let mut emit =
            |d: &[u8], s: bool, t: Instant| packets.borrow_mut().push((d.to_vec(), s, t));
        let mut f = Feeder {
            base: Instant::now(),
            frames: 0,
        };
        // 先有一段声音再停顿，编码器处于"正在使用"的状态；然后在一帧中间（不对齐包边界）突然出声
        let mut phase = 0.0;
        f.push(
            &mut p,
            &tone(FRAME_SAMPLES * 10, 0.3, &mut phase),
            &mut emit,
        );
        f.push(
            &mut p,
            &vec![0.0; FRAME_SAMPLES * 2 * 30 + 700 * 2],
            &mut emit,
        );
        let onset = f.at();
        f.push(
            &mut p,
            &tone(FRAME_SAMPLES * 10, 0.5, &mut phase),
            &mut emit,
        );

        let mut dec = opus::Decoder::new(AUDIO_SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES * 2];
        let (mut found, mut after_gap) = (None, false);
        for (pkt, start, t) in packets.take() {
            let n = dec.decode_float(&pkt, &mut out, false).unwrap();
            // 跳过开头那段声音，从停顿之后的 start 包开始找（出声可能落在下一包：解码输出晚一个前瞻）
            after_gap |= start && ms(t, onset) > -100.0;
            if !after_gap || found.is_some() {
                continue;
            }
            if let Some(i) = out[..n * 2].iter().position(|s| s.abs() > 0.05) {
                found = Some(t + frames_dur(i / 2));
            }
        }
        let got = found.expect("没有解码出声音");
        let err = ms(got, onset);
        println!("解码后出声时刻与真实采集时刻相差 {err:.2} ms");
        assert!(err.abs() < 3.0, "声音时间戳偏差 {err:.2} ms");
    }

    /// 缺少系统权限不算采集失败（另有提示）；其它原因原样告诉控制端
    #[test]
    fn failure_details() {
        let e = anyhow::anyhow!("no default audio output device");
        assert_eq!(
            failure_detail(&e).as_deref(),
            Some("no default audio output device")
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            failure_detail(&anyhow::Error::from(crate::host::macos::PermissionDenied)),
            None
        );
    }

    /// 订阅时就能拿到当前的状态；之后只有状态真的变了才通知（每次重试失败的原因一样时不重复发）
    #[tokio::test]
    async fn failure_is_reported_to_subscribers() {
        let shared = Arc::new(Shared {
            subs: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            failure: watch::Sender::new(None),
        });
        set_failure(&shared, Some("no default audio output device".into()));
        let mut sub = AudioSubscription::new(shared.clone(), 0);
        assert_eq!(
            sub.failure().as_deref(),
            Some("no default audio output device")
        );
        set_failure(&shared, Some("no default audio output device".into()));
        assert!(!sub.failure.has_changed().unwrap(), "原因没变不应通知");
        let wait = Duration::from_millis(50);
        assert!(
            tokio::time::timeout(wait, sub.failure_changed())
                .await
                .is_err()
        );
        set_failure(&shared, None);
        assert_eq!(
            tokio::time::timeout(wait, sub.failure_changed())
                .await
                .unwrap(),
            None
        );
        set_failure(&shared, Some("PulseAudio error 6".into()));
        assert_eq!(
            tokio::time::timeout(wait, sub.failure_changed())
                .await
                .unwrap()
                .as_deref(),
            Some("PulseAudio error 6")
        );
    }

    #[test]
    fn hub_starts_and_stops() {
        let (tx, _rx) = mpsc::channel(QUEUE);
        let a = subscribe(tx.clone());
        let b = subscribe(tx);
        assert!(Arc::ptr_eq(&a.shared, &b.shared), "同时订阅应共享采集线程");
        drop(a);
        assert!(!b.shared.stop.load(Ordering::Acquire));
        let shared = b.shared.clone();
        drop(b);
        assert!(
            shared.stop.load(Ordering::Acquire),
            "最后一个订阅者离开后应停止"
        );
        assert!(HUB.lock().unwrap().upgrade().is_none());
    }
}
