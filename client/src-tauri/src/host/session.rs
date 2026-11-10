//! 被控端的单个远控会话：桌面版的 [`Platform`]。协议层的事（同意流程、Pong、失联检测、踢人时发 Bye、
//! 画质设置的校验）在 [`airhand_link::host::serve`]，这里只管采集、编码、注入输入、剪贴板、声音，
//! 以及定时检查权限和显示器有没有变化。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_link::flow::Flow;
use airhand_link::host::{
    Acks, Admission, CursorCache, Incoming, Outbox, Platform, Request, Senders, Start, serve,
};
use airhand_proto::session::{DisplayInfo, HostHello, HostMsg, Notice, QualitySettings};
use anyhow::Result;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::{Interval, MissedTickBehavior};
use tracing::{info, warn};

use super::audio::{self, AudioSubscription};
use super::capture;
use super::gate::DesktopGate;
use super::input::InjectorThread;
use super::pipeline::Pipeline;
use crate::clipboard::{self, ClipSync};
use crate::config;

/// 画面的发送队列：在途帧控制之下最多一两帧，多了宁可让编码器等着
const VIDEO_QUEUE: usize = 2;

/// 告诉控制端的平台（控制端据此选快捷键、提示的说法）
#[cfg(windows)]
const PLATFORM: airhand_proto::session::Platform = airhand_proto::session::Platform::Windows;
#[cfg(target_os = "macos")]
const PLATFORM: airhand_proto::session::Platform = airhand_proto::session::Platform::Mac;
#[cfg(not(any(windows, target_os = "macos")))]
const PLATFORM: airhand_proto::session::Platform = airhand_proto::session::Platform::Linux;
/// 检查系统权限是否有变化的间隔
const PERMISSION_CHECK: Duration = Duration::from_secs(2);
/// 检查显示器有没有变化（插拔、旋转、改分辨率 / 缩放 / 排列）的间隔
const DISPLAY_CHECK: Duration = Duration::from_secs(2);

/// 显示器列表变了之后，正在看的显示器在新列表里的序号：按名字找（插拔别的显示器时序号可能变），
/// 找不到（被拔掉了）就改看主显示器（新列表的第一个）。
fn follow_display(old: &[DisplayInfo], current: u8, new: &[DisplayInfo]) -> u8 {
    old.get(current as usize)
        .and_then(|d| new.iter().position(|n| n.name == d.name))
        .unwrap_or(0) as u8
}

/// 等到声音采集的状态有变化；没有订阅声音时一直等下去
async fn audio_failure(sound: &mut Option<AudioSubscription>) -> Option<String> {
    match sound {
        Some(s) => s.failure_changed().await,
        None => std::future::pending().await,
    }
}

/// `(缺少屏幕录制, 缺少辅助功能)` → 提示（两项都为 false 表示已经全部授予）
fn permission_notice((screen, input): (bool, bool)) -> Notice {
    Notice::MissingPermissions { screen, input }
}

/// 发一条提示
fn notice(n: Notice) -> HostMsg {
    HostMsg::Notice(Some(n))
}

/// 列不出显示器时注入输入用的
fn fallback_display() -> DisplayInfo {
    DisplayInfo {
        index: 0,
        name: String::new(),
        x: 0,
        y: 0,
        width: 1,
        height: 1,
        primary: true,
    }
}

fn ticker(period: Duration) -> Interval {
    let mut t = tokio::time::interval(period);
    t.set_missed_tick_behavior(MissedTickBehavior::Skip);
    t
}

pub async fn run(inc: Incoming, gate: Arc<DesktopGate>) -> Result<()> {
    let session_id = inc.session_id;
    let ip = inc.peer.ip;
    // 端到端握手：没配对过、又没有配对码的控制端，在这里就被拒绝了（见 gate）。
    // 进来的时候控制端已经确认过了（重放的握手消息走不到这里），这时才在托盘里显示它、开始采集
    serve(
        inc,
        &*gate,
        VIDEO_QUEUE,
        audio::QUEUE,
        |admitted, senders| {
            // 桌面被控端不请本机用户同意（无人值守），gate 不会要求它
            debug_assert_ne!(admitted.admission, Admission::NeedsApproval);
            let mut desktop = Desktop::new(session_id, senders);
            desktop.identity = Some((gate.clone(), ip, admitted.clone()));
            desktop
        },
    )
    .await
}

/// 桌面版一个会话里的东西
struct Desktop {
    session_id: u64,
    /// 认证完成后暂存，只有 start 成功才发布为活动控制端。
    identity: Option<(
        Arc<DesktopGate>,
        std::net::IpAddr,
        airhand_link::host::Admitted,
    )>,
    out: Outbox,
    video: mpsc::Sender<Bytes>,
    audio: mpsc::Sender<Bytes>,
    displays: Vec<DisplayInfo>,
    /// 正在看的显示器（`displays` 里的序号）
    current: u8,
    settings: QualitySettings,
    flow: Arc<Flow>,
    /// 控制端缓存了哪些光标形状：整个会话一份，换显示器（重建管线）时接着用
    cursors: Arc<Mutex<CursorCache>>,
    pipeline: Option<Pipeline>,
    /// 注入输入的线程：注入可能很慢（X11 上输入文字），会话的任务不等它
    injector: InjectorThread,
    sound: Option<AudioSubscription>,
    clip: ClipSync,
    clip_tick: Interval,
    permission_tick: Interval,
    display_tick: Interval,
    /// 缺少的系统权限（macOS），见 [`super::missing_permissions`]
    missing: (bool, bool),
}

/// 会话开始之后定时要做的事
enum Tick {
    Clipboard,
    /// 声音采集打不开（`Some`）/ 恢复了
    Audio(Option<String>),
    Permissions,
    Displays,
}

impl Desktop {
    fn new(session_id: u64, senders: Senders) -> Self {
        Self {
            session_id,
            identity: None,
            out: senders.out,
            video: senders.video,
            audio: senders.audio,
            displays: Vec::new(),
            current: 0,
            settings: QualitySettings::default(),
            flow: Arc::new(Flow::default()),
            cursors: Arc::default(),
            pipeline: None,
            injector: InjectorThread::new(),
            sound: None,
            clip: ClipSync::new(),
            clip_tick: ticker(clipboard::POLL),
            permission_tick: ticker(PERMISSION_CHECK),
            display_tick: ticker(DISPLAY_CHECK),
            missing: (false, false),
        }
    }

    /// 停掉现在的编码管线（新管线从关键帧开始，旧的编码帧不能夹在中间），在 `current` 上重新开始
    async fn restart_pipeline(&mut self) {
        self.injector.release_all();
        if let Some(p) = self.pipeline.take() {
            p.stop().await;
        }
        self.pipeline = Some(Pipeline::start(
            self.current,
            self.settings,
            self.video.clone(),
            self.out.clone(),
            self.flow.clone(),
            self.cursors.clone(),
        ));
    }

    fn subscribe_audio(&mut self) -> Result<()> {
        let s = audio::subscribe(self.audio.clone());
        // 别的会话已经在采集、并且正打不开：马上告诉这个控制端
        if let Some(detail) = s.failure() {
            self.out.send(&notice(Notice::AudioFailed { detail }))?;
        }
        self.sound = Some(s);
        Ok(())
    }
}

impl Platform for Desktop {
    type Event = Tick;

    /// 画面确认直接交给会话的在途控制（读任务里调用，不等这里换显示器、重建管线）
    fn acks(&self) -> Arc<dyn Acks> {
        self.flow.clone()
    }

    async fn start(&mut self, start: Start) -> Result<()> {
        let (displays, codecs, env_notice) = tokio::task::spawn_blocking(|| {
            (
                capture::displays(),
                super::encoder::available_codecs(),
                capture::environment_notice(),
            )
        })
        .await
        .unwrap_or_default();
        self.displays = displays;
        let (hostname, os) = crate::sysinfo::host_identity();
        info!(
            session_id = self.session_id,
            displays = self.displays.len(),
            "desktop session starting"
        );
        self.out.send(&HostMsg::Hello(HostHello {
            hostname,
            os,
            version: config::VERSION.to_string(),
            displays: self.displays.clone(),
            current_display: self.current,
            codecs,
            platform: PLATFORM,
            // 桌面被控端只有鼠标键盘：不接收触摸，也没有手机那样的系统操作（快捷键由控制端发按键）
            touch: false,
            actions: Vec::new(),
        }))?;
        if self.displays.is_empty() {
            self.out.send(&notice(Notice::NoDisplays))?;
        }
        if let Some(n) = env_notice {
            self.out.send(&notice(n))?;
        }
        // 缺少系统权限（macOS）：告诉控制端为什么没有画面 / 不能操作；之后权限有变化时再发
        self.missing = super::missing_permissions();
        if self.missing != (false, false) {
            self.out.send(&notice(permission_notice(self.missing)))?;
        }
        // 开始时的画质在握手里：编码器一开始就按它建，不用先建一个默认的、等设置到了再重建
        self.settings = start.settings;
        if !self.displays.is_empty() {
            self.restart_pipeline().await;
        }
        if start.audio {
            self.subscribe_audio()?;
        }
        if let Some((gate, ip, admitted)) = self.identity.take() {
            gate.admitted(self.session_id, ip, &admitted);
        }
        Ok(())
    }

    async fn request(&mut self, req: Request) -> Result<()> {
        match req {
            // 晚到太久的输入已经换成了"释放所有按键"。交给注入线程就返回：在这里等它注入完的话，
            // 这期间 Pong、画面的确认、本机用户的断开都处理不了（见 input::thread）
            Request::Input(events) => {
                let fallback = fallback_display();
                let d = self
                    .displays
                    .get(self.current as usize)
                    .unwrap_or(&fallback);
                self.injector.handle(events, d);
            }
            Request::Settings(s) => {
                self.settings = s;
                if let Some(p) = &self.pipeline {
                    p.update_settings(s);
                }
            }
            Request::Keyframe => {
                if let Some(p) = &self.pipeline {
                    p.request_keyframe();
                }
            }
            Request::SelectDisplay(index) => {
                if (index as usize) < self.displays.len() && index != self.current {
                    self.current = index;
                    self.restart_pipeline().await;
                    self.out.send(&HostMsg::Displays {
                        displays: self.displays.clone(),
                        current: self.current,
                    })?;
                }
            }
            Request::Clipboard(text) => self.clip.apply(text),
            Request::Audio(enabled) => {
                if !enabled {
                    self.sound = None;
                } else if self.sound.is_none() {
                    self.subscribe_audio()?;
                }
            }
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Tick {
        tokio::select! {
            _ = self.clip_tick.tick() => Tick::Clipboard,
            // 声音采集打不开 / 恢复了（只在控制端开着声音时）
            failure = audio_failure(&mut self.sound) => Tick::Audio(failure),
            _ = self.permission_tick.tick() => Tick::Permissions,
            _ = self.display_tick.tick() => Tick::Displays,
        }
    }

    async fn event(&mut self, tick: Tick) -> Result<()> {
        let session_id = self.session_id;
        match tick {
            Tick::Clipboard => {
                // 走大块通道：1 MiB 的剪贴板不挡控制消息。网络慢、前面排满了就丢掉这一次
                if let Some(text) = self.clip.poll()
                    && let Err(e) = self.out.send(&HostMsg::Clipboard { text })
                {
                    warn!(session_id, "dropped a clipboard update: {e:#}");
                }
            }
            Tick::Audio(failure) => {
                self.out.send(&notice(match failure {
                    Some(detail) => Notice::AudioFailed { detail },
                    None => Notice::AudioRecovered,
                }))?;
            }
            // 例如本机用户在会话中途打开了"辅助功能"：更新（两项都授予时撤下）控制端的提示
            Tick::Permissions => {
                let now = super::missing_permissions();
                if now != self.missing {
                    info!(
                        session_id,
                        screen = !now.0,
                        input = !now.1,
                        "permissions changed"
                    );
                    self.missing = now;
                    self.out.send(&notice(permission_notice(now)))?;
                }
            }
            // 显示器变了：鼠标坐标是按这里的列表换算的，旧列表会让点击落到别的位置
            // （例如把显示器转成竖屏后宽高对调）。更新列表并告诉控制端；采集线程自己会重新打开。
            // 正在看的显示器换了序号就跟着它走，被拔掉了就改看主显示器
            Tick::Displays => {
                let now = tokio::task::spawn_blocking(capture::displays)
                    .await
                    .unwrap_or_default();
                // 列不出来（例如切换显示模式的瞬间）时保留原来的列表
                if !now.is_empty() && now != self.displays {
                    let next = follow_display(&self.displays, self.current, &now);
                    info!(session_id, ?now, current = next, "displays changed");
                    self.displays = now;
                    if next != self.current || self.pipeline.is_none() {
                        self.current = next;
                        self.restart_pipeline().await;
                    }
                    // 只留最新的一个（见 airhand_link::host::queue）
                    self.out.send(&HostMsg::Displays {
                        displays: self.displays.clone(),
                        current: self.current,
                    })?;
                }
            }
        }
        Ok(())
    }

    fn video_stalled(&self) -> bool {
        self.flow.ack_stalled()
    }

    async fn stop(mut self) {
        self.injector.cancel();
        // 先停掉还拿着画面 / 声音发送端的东西，写任务才能把排着队的（例如 Bye）发完
        if let Some(p) = self.pipeline.take() {
            p.stop().await;
        }
        self.sound = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(index: u8, name: &str, x: i32, w: u32, h: u32) -> DisplayInfo {
        DisplayInfo {
            index,
            name: name.into(),
            x,
            y: 0,
            width: w,
            height: h,
            primary: index == 0,
        }
    }

    #[test]
    fn follows_the_current_display() {
        let old = [
            d(0, "DISPLAY1", 0, 1920, 1080),
            d(1, "DISPLAY2", 1920, 1920, 1080),
        ];
        // 转成竖屏：序号不变
        let rotated = [
            d(0, "DISPLAY1", 0, 1920, 1080),
            d(1, "DISPLAY2", 1920, 1080, 1920),
        ];
        assert_eq!(follow_display(&old, 1, &rotated), 1);
        // 在左边插了一个显示器，正在看的那个排到了后面
        let plugged = [
            d(0, "DISPLAY1", 0, 1920, 1080),
            d(1, "DISPLAY3", 1920, 1280, 1024),
            d(2, "DISPLAY2", 3200, 1920, 1080),
        ];
        assert_eq!(follow_display(&old, 1, &plugged), 2);
        // 正在看的被拔掉了：改看主显示器
        assert_eq!(follow_display(&old, 1, &old[..1]), 0);
        // 序号本来就不对
        assert_eq!(follow_display(&old, 7, &old), 0);
    }
}
