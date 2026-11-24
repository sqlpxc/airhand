//! 编码管线（每个会话的每一路画面一个线程，见 `airhand_proto::session` 的"多屏"）：取最新帧 → 节流 → 在途控制 → 编码 → 发送。
//!
//! 低延迟的关键：
//! * 永远只编码**最新**的一帧，旧帧直接丢弃，不排队；
//! * 帧率按画面的时间戳节流（见 [`super::pace`]），处理花的时间不会累积成延迟；
//! * 在途（已发送未确认）字节受限，网络变慢时编码器等待而不是把数据堆进缓冲区；
//! * 码率按网络上的排队和带宽估计每帧调整（见 [`airhand_link::flow`]）；
//! * 每一帧参考哪一帧写进画面包头；控制端等不到帧时，编码器支持长期参考帧就从控制端有的那一帧恢复（参考帧失效），
//!   不支持就编关键帧（见 [`airhand_link::refs`]）。
//!
//! 画面停下来之后补画质：画面大面积变化（切换窗口、滚动）时，码率控制只能给这一帧分到大约一帧的码率，
//! 文字是糊的；采集只在画面变化时才有新帧，不补的话控制端就一直停在这一帧上。把最后一帧再编几次，
//! 编码器只需要补残差，几帧之内就接近原画（实测 VideoToolbox H.264、4 Mbps、2880×1800 缩到 1080p 的文字：
//! 42 dB → 58 dB，补的几帧一共约 40 KB）。编码器偶尔丢掉的帧（返回空）也靠这一步补发。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use airhand_link::flow::Flow;
use airhand_link::host::{Outbox, SendError};
use airhand_link::refs::Plan;
use airhand_proto::media::{self, VideoHeader};
use airhand_proto::session::{HostMsg, Notice, QualitySettings};
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::capture::{self, Frame};
use super::cursor::{Reported, SharedCursor};
use super::encoder::{Encoder, LtrControl};
use super::pace::Pacer;

/// 画面停下来之后，同一帧最多再编这么多次
const REFINE_FRAMES: u8 = 8;
/// 画面静止这么久之后才开始补：画面还在连续变化（打字、播放）时不和新画面抢编码器
const REFINE_AFTER: Duration = Duration::from_millis(200);
/// 补的这一帧不到这么大，说明已经没什么可补的了
const REFINE_DONE_BYTES: usize = 512;

pub struct Pipeline {
    ctrl: Arc<Ctrl>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct Ctrl {
    settings: Mutex<QualitySettings>,
    settings_ver: AtomicU64,
    force_key: AtomicBool,
    /// 最近发出去的关键帧的序号（0：还没有）：控制端要的关键帧已经在路上了就不再编（见 [`Pipeline::request_keyframe`]）
    last_key: AtomicU32,
    stop: AtomicBool,
    flow: Arc<Flow>,
    /// 会话的光标（整个会话一份，几路画面谁报的算数见 [`SharedCursor`]）
    cursor: Arc<Mutex<SharedCursor>>,
}

/// 一路画面的编码管线要的东西
pub struct Config {
    /// 第几路画面（写进画面包头和编码器信息）
    pub stream: u8,
    /// 看哪个显示器
    pub display: u8,
    /// 这一路的画质（短边上限已经和这一路按显示的大小要的取了较严的）
    pub settings: QualitySettings,
    /// 这一路画面的发送队列
    pub video: mpsc::Sender<Bytes>,
    /// 控制消息（编码器、采集失败和光标状态走控制通道，光标图像走大块通道：形状图像有几十 KB，不能挡控制消息）
    pub out: Outbox,
    /// 这一路的在途控制和码率（会话一开始就建好，换显示器时接着用）
    pub flow: Arc<Flow>,
    pub cursor: Arc<Mutex<SharedCursor>>,
}

impl Pipeline {
    pub fn start(config: Config) -> Self {
        let Config {
            stream,
            display,
            settings,
            video,
            out,
            flow,
            cursor,
        } = config;
        let ctrl = Arc::new(Ctrl {
            settings: Mutex::new(settings),
            settings_ver: AtomicU64::new(0),
            force_key: AtomicBool::new(true),
            last_key: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            flow,
            cursor,
        });
        let c = ctrl.clone();
        let thread = std::thread::Builder::new()
            .name(format!("airhand-encoder-{stream}"))
            .spawn(move || run(c, stream, display, video, out))
            .expect("spawn encoder");
        Self {
            ctrl,
            thread: Some(thread),
        }
    }

    pub fn update_settings(&self, s: QualitySettings) {
        *self.ctrl.settings.lock().unwrap() = s;
        self.ctrl.settings_ver.fetch_add(1, Ordering::Release);
    }

    /// 要一个关键帧。`after`：控制端收到的最后一帧：在那之后已经发过关键帧（还在路上）就不再编。
    /// 解码出错、换路缺了帧、界面切回来各自会要，一个往返里的几次请求只编一个；`None`：一定要编
    pub fn request_keyframe(&self, after: Option<u32>) {
        let sent = self.ctrl.last_key.load(Ordering::Acquire);
        if let Some(after) = after
            && sent != 0
            && sent.wrapping_sub(after) as i32 > 0
        {
            return;
        }
        self.ctrl.force_key.store(true, Ordering::Release);
    }

    /// Join off the async worker before replacing a pipeline: old encoded frames
    /// must not interleave with the new encoder's keyframe.
    pub async fn stop(mut self) {
        self.ctrl.stop.store(true, Ordering::Release);
        self.ctrl.flow.wake();
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.ctrl.stop.store(true, Ordering::Release);
        self.ctrl.flow.wake();
    }
}

// ───────────────────────────── 编码线程 ─────────────────────────────

fn send_cancellable(out: &mpsc::Sender<Bytes>, stop: &AtomicBool, mut bytes: Bytes) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match out.try_send(bytes) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(b)) => {
                bytes = b;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// 发一条控制消息；控制通道排满了（网络跟不上）就等着，直到管线被停下
fn send_msg_cancellable(out: &Outbox, stop: &AtomicBool, msg: &HostMsg) -> bool {
    let msg = Outbox::encode(msg);
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match out.send_encoded(msg.clone()) {
            Ok(()) => return true,
            Err(SendError::Closed | SendError::TooLarge) => return false,
            Err(SendError::Full) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// 关掉编码器。卡住了的硬件编码器关掉时可能要等一会儿（这期间这一路画面停着）：太久的记一笔
fn drop_encoder(enc: &mut Option<Encoder>) {
    let started = Instant::now();
    *enc = None;
    let took = started.elapsed();
    if took > Duration::from_millis(500) {
        warn!(
            took_ms = took.as_millis() as u64,
            "closing the encoder took long"
        );
    }
}

fn run(ctrl: Arc<Ctrl>, stream: u8, screen: u8, video: mpsc::Sender<Bytes>, out: Outbox) {
    capture::raise_priority();
    let mut settings = *ctrl.settings.lock().unwrap();
    let mut ver = ctrl.settings_ver.load(Ordering::Acquire);
    // 控制端的游戏模式要光标画进画面：读带光标的那一路（见 capture 的"带光标的画面"）
    let mut draw = settings.draw_cursor;
    let mut sub = capture::subscribe(screen, settings.fps, settings.max_short, draw);
    let mut enc: Option<Encoder> = None;
    // 硬件编码器连续失败时改用软件编码；GPU 零拷贝连续失败时硬件编码器改用内存里的画面
    let mut hw_failures = 0u32;
    let mut gpu_failures = 0u32;
    let (mut frame_seq, mut cursor_seq, mut shape_seq) = (0u64, 0u64, 0u64);
    // 光标有变化还没发出去（大块通道的队列满了）：下一轮再发最新的
    let mut cursor_pending = false;
    let mut last_frame: Option<Arc<Frame>> = None;
    let mut pacer = Pacer::new(settings.fps);
    // 码率估计属于会话（换显示器重建管线时接着用），画质设置里的码率是上限
    ctrl.flow
        .set_target(settings.bitrate_kbps, settings.adaptive);
    let mut reported_error: Option<String> = None;
    // 补画质：还要补几次、最早什么时候补
    let mut refine_left = 0u8;
    let mut refine_due = Instant::now();
    info!(stream, screen, ?settings, "encoding pipeline started");

    // 控制通道的队列满了（网络跟不上）就等着，直到管线被停下
    let send = |msg: &HostMsg| send_msg_cancellable(&out, &ctrl.stop, msg);
    let notice = |n: Notice| send(&HostMsg::Notice(Some(n)));

    while !ctrl.stop.load(Ordering::Acquire) {
        let v = ctrl.settings_ver.load(Ordering::Acquire);
        if v != ver {
            ver = v;
            let prev_short = settings.max_short;
            settings = *ctrl.settings.lock().unwrap();
            // 只有 macOS 在采集时缩放，其余平台改分辨率不用换采集
            let resized = cfg!(target_os = "macos") && settings.max_short != prev_short;
            if resized || settings.draw_cursor != draw {
                // 换一路采集：旧的画面不再用，等新的一路的第一帧（采集一开始就会给）；光标状态重新发。
                // 只是要不要光标变了：画面只差一个光标，接着编增量帧，不用关键帧
                info!(
                    max_short = settings.max_short,
                    draw = settings.draw_cursor,
                    "switching the capture"
                );
                if resized {
                    ctrl.force_key.store(true, Ordering::Release);
                }
                draw = settings.draw_cursor;
                sub = capture::subscribe(screen, settings.fps, settings.max_short, draw);
                (frame_seq, cursor_seq, shape_seq) = (0, 0, 0);
                last_frame = None;
                refine_left = 0;
            }
            sub.set_fps(settings.fps);
            pacer.set_fps(settings.fps);
            ctrl.flow
                .set_target(settings.bitrate_kbps, settings.adaptive);
            if let Some(e) = &mut enc {
                e.set_bitrate(ctrl.flow.bitrate_kbps());
                // 需要重建编码器（例如换了编码格式）：用上一帧马上重建，画面静止时也立即生效，
                // 控制端也能马上收到新的 HostMsg::Encoder（界面上显示的是实际在用的格式）
                if let Some(f) = &last_frame
                    && !e.compatible(f, &settings)
                {
                    ctrl.force_key.store(true, Ordering::Release);
                }
            }
            info!(?settings, "quality settings updated");
        }

        let wait = if refine_left > 0 {
            refine_due
                .saturating_duration_since(Instant::now())
                .clamp(Duration::from_millis(1), Duration::from_millis(40))
        } else {
            Duration::from_millis(40)
        };
        let slot = sub.slot();
        let (fs, cs) = slot.wait_change(frame_seq, cursor_seq, wait, draw);

        // 光标变化：可见性和形状引用走控制通道（只留最新值）；控制端还没有的形状（动画光标第一次转到的帧）
        // 先把图像排进大块通道，和剪贴板共用：队列满了不等，下一轮再发最新形状（见 CursorCache）。
        // 同时看好几个显示器时只听光标所在的那一路（见 SharedCursor）
        if cs > cursor_seq {
            cursor_seq = cs;
            cursor_pending = true;
        }
        if cursor_pending {
            let c = slot.cursor();
            let shape = match &c.shape {
                Some(s) if c.shape_seq > shape_seq => Some(s.as_ref()),
                _ => None,
            };
            let reported =
                ctrl.cursor
                    .lock()
                    .unwrap()
                    .report(stream, c.here, &out, c.visibility, shape);
            match reported {
                Ok(Reported::Sent) => {
                    shape_seq = c.shape_seq;
                    cursor_pending = false;
                }
                // 不归这一路管：之后归它管的时候形状要重发一遍（别的一路可能换过形状）
                Ok(Reported::Ignored) => {
                    shape_seq = 0;
                    cursor_pending = false;
                }
                Err(e) => {
                    // 没发出去的图像由 CursorCache 忘掉；可见性已经独立发送。
                    if matches!(e, SendError::Closed | SendError::TooLarge) {
                        break;
                    }
                }
            }
        }

        let force = ctrl.force_key.load(Ordering::Acquire);
        if fs <= frame_seq {
            if let Some(err) = slot.error()
                && reported_error.as_ref() != Some(&err)
            {
                if !notice(Notice::CaptureFailed {
                    detail: err.clone(),
                }) {
                    break;
                }
                reported_error = Some(err);
            }
            // 画面静止但控制端需要关键帧（例如解码器重置）、等不到帧了要恢复，或者该补画质了：重新编码上一帧
            let refine = refine_left > 0 && Instant::now() >= refine_due;
            let recover = ctrl.flow.recovering();
            if !((force || refine || recover) && last_frame.is_some()) {
                continue;
            }
        } else if reported_error.take().is_some() {
            // 报告过采集失败，现在又有新画面了：让控制端撤下提示
            if !notice(Notice::CaptureRecovered) {
                break;
            }
        }

        // 帧率节流：按画面的时间戳排时间线（见 pace），不按上一帧编完的时刻
        let wait = pacer.wait(Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        // 在途控制：网络跟不上时在这里等待（期间新的画面会覆盖旧画面，不会排队）
        while !ctrl
            .flow
            .wait_window(settings.fps, &ctrl.stop, Duration::from_millis(50))
        {
            if ctrl.stop.load(Ordering::Acquire) {
                return;
            }
        }
        // 取等待期间到达的最新一帧；没有的话重新编码上一帧（关键帧、补画质）
        let (frame, fresh) = match slot.try_newer(frame_seq, draw) {
            Some((s, f)) => {
                frame_seq = s;
                (f, true)
            }
            None => match &last_frame {
                Some(f) => (f.clone(), false),
                None => continue,
            },
        };
        let started = Instant::now();
        pacer.done(if fresh { frame.captured } else { started });

        let mut force = ctrl.force_key.swap(false, Ordering::AcqRel);
        if !enc
            .as_ref()
            .is_some_and(|e| e.compatible(&frame, &settings))
        {
            drop_encoder(&mut enc);
            match Encoder::new(
                &frame,
                &settings,
                ctrl.flow.bitrate_kbps(),
                hw_failures < 3,
                gpu_failures < 2,
            ) {
                Ok(e) => {
                    let mut info = e.info().clone();
                    info.stream = stream;
                    if !send(&HostMsg::Encoder(info)) {
                        break;
                    }
                    enc = Some(e);
                    force = true;
                }
                Err(e) => {
                    warn!("can't create the encoder: {e:#}");
                    ctrl.force_key.store(true, Ordering::Release);
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        }
        let e = enc.as_mut().unwrap();
        // 不自适应时就是画质设置里的码率；编码器自己会跳过没变的
        e.set_bitrate(ctrl.flow.bitrate_kbps());
        // 参考帧：控制端等不到帧时从它有的长期参考帧恢复（做不到就编关键帧），平时隔一阵标一个长期参考帧
        let plan = ctrl.flow.plan(e.ltr_slots());
        force |= plan.key;
        let plan = if force {
            Plan {
                key: true,
                ..Plan::default()
            }
        } else {
            if plan.mark.is_some() || plan.recover.is_some() {
                e.set_ltr(LtrControl {
                    mark: plan.mark,
                    recover: plan.recover,
                });
            }
            plan
        };

        // 新画面、关键帧之后从头开始补；补的帧本身只减次数
        if fresh || force {
            refine_left = REFINE_FRAMES;
            refine_due = started + REFINE_AFTER;
        } else {
            refine_left = refine_left.saturating_sub(1);
        }
        match e.encode(&frame, force) {
            Ok(Some(ef)) => {
                if ctrl.stop.load(Ordering::Acquire) {
                    break;
                }
                let size = media::HEADER_LEN + ef.data.len();
                if size > media::MAX_VIDEO_PACKET {
                    notice(Notice::CaptureFailed {
                        detail: "encoded frame exceeds the video budget".into(),
                    });
                    break;
                }
                while !ctrl
                    .flow
                    .wait_room(size, &ctrl.stop, Duration::from_millis(50))
                {
                    if ctrl.stop.load(Ordering::Acquire) {
                        return;
                    }
                }
                let (seq, reference) = ctrl.flow.sent_frame(size, ef.key, plan, ef.ltr);
                if ef.key {
                    ctrl.last_key.store(seq, Ordering::Release);
                }
                let info = e.info();
                let len = ef.data.len();
                if !fresh && !force && len < REFINE_DONE_BYTES {
                    refine_left = 0;
                }
                let header = VideoHeader {
                    seq,
                    stream,
                    key: ef.key,
                    codec: info.codec,
                    width: info.width as u16,
                    height: info.height as u16,
                    // 重新编码的上一帧就是现在的画面（画面一直没变），采集时间记为现在：
                    // 控制端按采集时间做音画同步，用旧的时间会以为这一帧严重滞后，让声音跟着多等
                    capture_us: super::clock::us(if fresh { frame.captured } else { started }),
                    reference,
                };
                let packet = Bytes::from(media::frame(&header.encode(), &[&ef.data]));
                if !send_cancellable(&video, &ctrl.stop, packet) {
                    ctrl.flow.discard(seq);
                    break;
                }
            }
            Ok(None) => {
                if force {
                    ctrl.force_key.store(true, Ordering::Release);
                }
            }
            Err(err) => {
                warn!(
                    gpu = e.uses_gpu(),
                    "encoding failed, recreating the encoder: {err:#}"
                );
                if e.uses_gpu() {
                    gpu_failures += 1;
                } else if e.info().hardware {
                    hw_failures += 1;
                }
                drop_encoder(&mut enc);
                ctrl.force_key.store(true, Ordering::Release);
            }
        }
        last_frame = Some(frame);
    }
    info!(stream, screen, "encoding pipeline stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopping_encoder_interrupts_a_full_output_queue() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Bytes::from_static(b"old")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread =
            std::thread::spawn(move || send_cancellable(&tx, &flag, Bytes::from_static(b"new")));
        stop.store(true, Ordering::Release);
        assert!(!thread.join().unwrap());
    }

    /// 控制通道排满了：编码线程等着，直到有空位或者管线被停下；写任务不在了就不等了
    #[test]
    fn control_messages_wait_for_room_or_stop() {
        use airhand_link::host::queue;
        use airhand_proto::session::Notice;
        let (out, mut control, _bulk) = queue::channel();
        let n = HostMsg::Notice(Some(Notice::NoDisplays));
        while out.send(&n).is_ok() {}
        let stop = Arc::new(AtomicBool::new(false));
        let (o, s) = (out.clone(), stop.clone());
        let waiting = std::thread::spawn(move || send_msg_cancellable(&o, &s, &n));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!waiting.is_finished());
        // 写任务发走了一条：放进去了
        assert!(control.try_recv().is_some());
        assert!(waiting.join().unwrap());

        let n = HostMsg::Notice(Some(Notice::NoDisplays));
        let (o, s) = (out.clone(), stop.clone());
        let waiting = std::thread::spawn(move || send_msg_cancellable(&o, &s, &n));
        std::thread::sleep(Duration::from_millis(30));
        stop.store(true, Ordering::Release);
        assert!(!waiting.join().unwrap());

        drop(control);
        stop.store(false, Ordering::Release);
        assert!(!send_msg_cancellable(&out, &stop, &HostMsg::Notice(None)));
    }
}
