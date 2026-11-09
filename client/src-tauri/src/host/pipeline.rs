//! 编码管线（每个会话一个线程）：取最新帧 → 节流 → 在途帧控制 → 编码 → 发送。
//!
//! 低延迟的关键：
//! * 永远只编码**最新**的一帧，旧帧直接丢弃，不排队；
//! * 在途（已发送未确认）帧数量受限，网络变慢时编码器等待而不是把数据堆进缓冲区；
//! * 自适应码率根据阻塞情况与 RTT 膨胀程度下调/恢复码率。
//!
//! 画面停下来之后补画质：画面大面积变化（切换窗口、滚动）时，码率控制只能给这一帧分到大约一帧的码率，
//! 文字是糊的；采集只在画面变化时才有新帧，不补的话控制端就一直停在这一帧上。把最后一帧再编几次，
//! 编码器只需要补残差，几帧之内就接近原画（实测 VideoToolbox H.264、4 Mbps、2880×1800 缩到 1080p 的文字：
//! 42 dB → 58 dB，补的几帧一共约 40 KB）。编码器偶尔丢掉的帧（返回空）也靠这一步补发。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use airhand_link::flow::{Abr, Flow};
use airhand_link::host::{Control, CursorCache};
use airhand_proto::media::{self, VideoHeader};
use airhand_proto::session::{CursorState, HostMsg, Notice, QualitySettings};
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::capture::{self, Frame};
use super::encoder::Encoder;

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
    stop: AtomicBool,
    flow: Arc<Flow>,
    /// 控制端缓存了哪些光标形状（整个会话一份）
    cursors: Arc<Mutex<CursorCache>>,
}

impl Pipeline {
    /// `video`：画面流的发送队列；`control`：控制流（光标、编码器、采集失败的提示）；
    /// `cursors`：控制端缓存了哪些光标形状（动画光标的每一帧只发一次图像）
    pub fn start(
        display: u8,
        settings: QualitySettings,
        video: mpsc::Sender<Bytes>,
        control: Control,
        flow: Arc<Flow>,
        cursors: Arc<Mutex<CursorCache>>,
    ) -> Self {
        let ctrl = Arc::new(Ctrl {
            settings: Mutex::new(settings),
            settings_ver: AtomicU64::new(0),
            force_key: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            flow,
            cursors,
        });
        let c = ctrl.clone();
        let thread = std::thread::Builder::new()
            .name("airhand-encoder".into())
            .spawn(move || run(c, display, video, control))
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

    pub fn request_keyframe(&self) {
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

fn run(ctrl: Arc<Ctrl>, screen: u8, video: mpsc::Sender<Bytes>, control: Control) {
    capture::raise_priority();
    let mut settings = *ctrl.settings.lock().unwrap();
    let mut ver = ctrl.settings_ver.load(Ordering::Acquire);
    let sub = capture::subscribe(screen, settings.fps);
    let slot = sub.slot();
    let mut enc: Option<Encoder> = None;
    // 硬件编码器连续失败时改用软件编码
    let mut hw_failures = 0u32;
    let (mut frame_seq, mut cursor_seq, mut shape_seq) = (0u64, 0u64, 0u64);
    let mut last_frame: Option<Arc<Frame>> = None;
    let mut next_due = Instant::now();
    let mut abr = Abr::new(settings.bitrate_kbps);
    let mut reported_error: Option<String> = None;
    // 补画质：还要补几次、最早什么时候补
    let mut refine_left = 0u8;
    let mut refine_due = Instant::now();
    info!(screen, ?settings, "encoding pipeline started");

    // 队列满了（网络跟不上）就等着，直到管线被停下
    let send = |msg: &HostMsg| send_cancellable(control.queue(), &ctrl.stop, Control::encode(msg));

    while !ctrl.stop.load(Ordering::Acquire) {
        let v = ctrl.settings_ver.load(Ordering::Acquire);
        if v != ver {
            ver = v;
            settings = *ctrl.settings.lock().unwrap();
            sub.set_fps(settings.fps);
            abr.reset(settings.bitrate_kbps);
            if let Some(e) = &mut enc {
                e.set_bitrate(settings.bitrate_kbps);
                // 需要重建编码器（例如换了编码格式）：用上一帧马上重建，画面静止时也立即生效，
                // 控制端也能马上收到新的 HostMsg::Encoder（界面上显示的是实际在用的格式）
                if let Some(f) = &last_frame
                    && !e.compatible(f.width, f.height, &settings)
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
        let (fs, cs) = slot.wait_change(frame_seq, cursor_seq, wait);

        // 光标变化：只发送可见性 +（如有变化）形状。控制端已经有的形状（动画光标循环到的帧）只发槽位
        if cs > cursor_seq {
            cursor_seq = cs;
            let c = slot.cursor();
            let shape = match &c.shape {
                Some(s) if c.shape_seq > shape_seq => Some(ctrl.cursors.lock().unwrap().place(s)),
                _ => None,
            };
            shape_seq = c.shape_seq;
            let msg = HostMsg::Cursor(CursorState {
                visible: c.visible,
                shape,
            });
            if !send(&msg) {
                // 没发出去：控制端没有这个形状，下次还得带图像
                if let HostMsg::Cursor(CursorState { shape: Some(s), .. }) = &msg {
                    ctrl.cursors.lock().unwrap().forget(s);
                }
                break;
            }
        }

        let force = ctrl.force_key.load(Ordering::Acquire);
        if fs <= frame_seq {
            if let Some(err) = slot.error()
                && reported_error.as_ref() != Some(&err)
            {
                if !send(&HostMsg::Notice(Notice::CaptureFailed {
                    detail: err.clone(),
                })) {
                    break;
                }
                reported_error = Some(err);
            }
            // 画面静止但控制端需要关键帧（例如解码器重置），或者该补画质了：重新编码上一帧
            let refine = refine_left > 0 && Instant::now() >= refine_due;
            if !((force || refine) && last_frame.is_some()) {
                if settings.adaptive {
                    let (srtt, min_rtt) = ctrl.flow.rtt();
                    abr.tick(srtt, min_rtt);
                }
                continue;
            }
        } else if reported_error.take().is_some() {
            // 报告过采集失败，现在又有新画面了：让控制端撤下提示
            if !send(&HostMsg::Notice(Notice::CaptureRecovered)) {
                break;
            }
        }

        // 帧率节流
        let now = Instant::now();
        if now < next_due {
            std::thread::sleep(next_due - now);
        }
        // 在途帧控制：网络跟不上时在这里等待（期间新的画面会覆盖旧画面，不会排队）
        while !ctrl
            .flow
            .wait_window(settings.fps, &ctrl.stop, Duration::from_millis(50))
        {
            abr.blocked += 1;
            if ctrl.stop.load(Ordering::Acquire) {
                return;
            }
        }
        // 取等待期间到达的最新一帧；没有的话重新编码上一帧（关键帧、补画质）
        let (frame, fresh) = match slot.try_newer(frame_seq) {
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
        next_due = started + Duration::from_secs_f64(1.0 / settings.fps.max(1) as f64);

        let mut force = ctrl.force_key.swap(false, Ordering::AcqRel);
        if !enc
            .as_ref()
            .is_some_and(|e| e.compatible(frame.width, frame.height, &settings))
        {
            enc = None;
            match Encoder::new(
                frame.width,
                frame.height,
                &settings,
                abr.current,
                hw_failures < 3,
            ) {
                Ok(e) => {
                    if !send(&HostMsg::Encoder(e.info().clone())) {
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
        if settings.adaptive {
            let (srtt, min_rtt) = ctrl.flow.rtt();
            abr.tick(srtt, min_rtt);
            e.set_bitrate(abr.current);
        }

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
                let seq = ctrl.flow.sent();
                let info = e.info();
                let len = ef.data.len();
                if !fresh && !force && len < REFINE_DONE_BYTES {
                    refine_left = 0;
                }
                let header = VideoHeader {
                    seq,
                    key: ef.key,
                    codec: info.codec,
                    width: info.width as u16,
                    height: info.height as u16,
                    // 重新编码的上一帧就是现在的画面（画面一直没变），采集时间记为现在：
                    // 控制端按采集时间做音画同步，用旧的时间会以为这一帧严重滞后，让声音跟着多等
                    capture_us: super::clock::us(if fresh { frame.captured } else { started }),
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
                warn!("encoding failed, recreating the encoder: {err:#}");
                if e.info().hardware {
                    hw_failures += 1;
                }
                enc = None;
                ctrl.force_key.store(true, Ordering::Release);
            }
        }
        last_frame = Some(frame);
    }
    info!(screen, "encoding pipeline stopped");
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
}
