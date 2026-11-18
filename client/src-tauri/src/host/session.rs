//! 被控端的单个远控会话：桌面版的 [`Platform`]。协议层的事（Pong、失联检测、踢人时发 Bye、
//! 画质设置的校验）在 [`airhand_link::host::serve`]，这里只管采集、编码、注入输入、剪贴板、声音、
//! 控制端的麦克风（见 [`super::mic`]），以及定时检查权限和显示器有没有变化。
//!
//! 控制端可以同时看好几个显示器（见 `airhand_proto::session` 的"多屏"）：每一路画面一条编码管线（[`Pipeline`]），
//! 各自的在途控制和码率（[`Flows`]）；采集按显示器和别的会话共用（见 [`capture`]），光标整个会话一份
//! （[`SharedCursor`]）。鼠标的绝对坐标按它说的那一路画面的显示器换算。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_link::host::{
    Acks, Flows, Incoming, MicSink, Outbox, Platform, Request, Senders, Start, resolve_view, serve,
};
use airhand_proto::MAX_STREAMS;
use airhand_proto::session::{
    DisplayInfo, HostHello, HostMsg, MicState, Notice, QualitySettings, StreamView, stricter_short,
};
use anyhow::Result;
use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use tokio::time::{Interval, MissedTickBehavior};
use tracing::{info, warn};

use super::audio::{self, AudioSubscription};
use super::capture;
use super::cursor::SharedCursor;
use super::gate::DesktopGate;
use super::input::InjectorThread;
use super::mic::{self, MicInput, MicLease};
use super::pipeline::{self, Pipeline};
use crate::clipboard::{self, ClipSync};
use crate::config;
use airhand_link::controller::Clip;

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
/// 没有虚拟麦克风（或者打不开）时隔多久再找一次，例如对方刚装好 Steam
const MIC_CHECK: Duration = Duration::from_secs(5);

/// 显示器列表变了之后，每一路画面看的显示器在新列表里的序号：按名字找（插拔别的显示器时序号可能变），
/// 找不到（被拔掉了）的那一路不要了；一路都不剩就改看主显示器（新列表的第一个，见 [`resolve_view`]）。
fn follow_view(old: &[DisplayInfo], view: &[StreamView], new: &[DisplayInfo]) -> Vec<StreamView> {
    let followed: Vec<StreamView> = view
        .iter()
        .filter_map(|v| {
            let name = &old.get(v.display as usize)?.name;
            let display = new.iter().position(|n| &n.name == name)? as u8;
            Some(StreamView { display, ..*v })
        })
        .collect();
    resolve_view(&followed, new.len(), MAX_STREAMS)
}

/// 等到声音采集的状态有变化；没有订阅声音时一直等下去
async fn audio_failure(sound: &mut Option<AudioSubscription>) -> Option<String> {
    match sound {
        Some(s) => s.failure_changed().await,
        None => std::future::pending().await,
    }
}

/// 等到麦克风（播放线程）的状态有变化；没有在用麦克风时一直等下去
async fn mic_changed(mic: &mut Option<MicLease>) -> mic::Status {
    match mic {
        Some(m) => m.changed().await,
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
    // 握手通过就开始采集（不等控制端的确认，省一个往返），确认了才在托盘里显示它（见 Platform::confirmed）
    serve(
        inc,
        &*gate,
        VIDEO_QUEUE,
        audio::QUEUE,
        |admitted, senders| {
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
    /// 认证完成后暂存，控制端确认了握手（[`Platform::confirmed`]）才发布为活动控制端。
    identity: Option<(
        Arc<DesktopGate>,
        std::net::IpAddr,
        airhand_link::host::Admitted,
    )>,
    out: Outbox,
    /// 每一路画面的发送队列（第 `i` 项是第 `i` 路）
    video: Vec<mpsc::Sender<Bytes>>,
    audio: mpsc::Sender<Bytes>,
    displays: Vec<DisplayInfo>,
    /// 每一路画面看哪个显示器（`displays` 里的序号）、按显示的大小要的短边上限：[`resolve_view`] 过的，
    /// 有显示器时至少一路
    view: Vec<StreamView>,
    settings: QualitySettings,
    /// 每一路画面的在途控制和码率（会话一开始就建好：确认在读任务里就交给它们）
    flows: Arc<Flows>,
    /// 光标：整个会话一份，换显示器（重建管线）时接着用；几路画面谁报的算数见 [`SharedCursor`]
    cursor: Arc<Mutex<SharedCursor>>,
    /// 每一路画面的编码管线（第 `i` 项是第 `i` 路，一共 [`MAX_STREAMS`] 项）
    pipelines: Vec<Option<Pipeline>>,
    /// 注入输入的线程：注入可能很慢（X11 上输入文字），会话的任务不等它
    injector: InjectorThread,
    sound: Option<AudioSubscription>,
    clip: ClipSync,
    clip_tick: Interval,
    /// 剪贴板里的文件（见 `airhand_link::files`）。
    files: Option<airhand_link::files::Files>,
    permission_tick: Interval,
    display_tick: Interval,
    /// 缺少的系统权限（macOS），见 [`super::missing_permissions`]
    missing: (bool, bool),
    mic: Mic,
}

/// 控制端的麦克风（见 [`super::mic`]）
struct Mic {
    /// 控制端麦克风的声音从读任务交到这里（打开了才交给播放线程）
    input: Arc<MicInput>,
    /// 控制端想开着麦克风：开不了（别的会话占着、没有设备）的话，等能开了自动打开
    wanted: bool,
    /// 控制端打开了麦克风、而且轮到了这个会话
    lease: Option<MicLease>,
    /// 告诉过控制端的状态（一样的不再发）
    sent: Option<mic::Status>,
    /// 用麦克风的会话变了（别的会话放开了）
    owner: watch::Receiver<Option<u64>>,
    /// 没有设备、打不开时定时再看看
    tick: Interval,
}

impl Mic {
    /// 没在用、现在也用不了：要等别的会话放开、设备出现
    fn waiting(&self) -> bool {
        mic::supported() && self.lease.is_none()
    }

    /// 现在用不了是因为设备（没有、打不开）：定时再找
    fn missing(&self) -> bool {
        self.waiting()
            && matches!(
                self.sent,
                Some((
                    MicState::NoDevice
                        | MicState::Failed
                        | MicState::SteamMissing
                        | MicState::SetupFailed,
                    _
                ))
            )
    }
}

/// 会话开始之后定时要做的事
enum Tick {
    Clipboard,
    /// 声音采集打不开（`Some`）/ 恢复了
    Audio(Option<String>),
    Permissions,
    Displays,
    /// 麦克风的播放线程的状态变了（打开了设备、打不开、设备没了……）
    Mic(mic::Status),
    /// 别的会话放开了麦克风，或者该再找一次设备了
    MicCheck,
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
            view: Vec::new(),
            settings: QualitySettings::default(),
            flows: Arc::default(),
            cursor: Arc::default(),
            pipelines: (0..MAX_STREAMS).map(|_| None).collect(),
            injector: InjectorThread::new(),
            sound: None,
            clip: ClipSync::new(),
            clip_tick: ticker(clipboard::POLL),
            files: senders.files,
            permission_tick: ticker(PERMISSION_CHECK),
            display_tick: ticker(DISPLAY_CHECK),
            missing: (false, false),
            mic: Mic {
                input: Arc::default(),
                wanted: false,
                lease: None,
                sent: None,
                owner: mic::owner_changed(),
                tick: ticker(MIC_CHECK),
            },
        }
    }

    /// 告诉控制端麦克风现在怎么样（和上次说的一样就不说了）
    fn send_mic(&mut self, status: mic::Status) -> Result<()> {
        if self.mic.sent.as_ref() == Some(&status) {
            return Ok(());
        }
        let (state, detail) = status.clone();
        self.out.send(&HostMsg::Mic { state, detail })?;
        self.mic.sent = Some(status);
        Ok(())
    }

    /// 控制端要开着麦克风：轮到这个会话了就打开，不然告诉它为什么开不了
    fn open_mic(&mut self) -> Result<()> {
        let initial = self.mic.sent.clone().unwrap_or((MicState::Preparing, None));
        let status = match mic::open(self.session_id, &self.mic.input, initial) {
            Ok(lease) => {
                let s = lease.status();
                self.mic.lease = Some(lease);
                s
            }
            Err(state) => (state, None),
        };
        self.send_mic(status)
    }

    /// 第 `stream` 路画面的画质：画质设置，短边上限和这一路按显示的大小要的取较严的
    fn stream_settings(&self, stream: usize) -> QualitySettings {
        let mut s = self.settings;
        if let Some(v) = self.view.get(stream) {
            s.max_short = stricter_short(s.max_short, v.max_short);
        }
        s
    }

    /// 第 0 路画面是哪个显示器
    fn current(&self) -> u8 {
        self.view.first().map_or(0, |v| v.display)
    }

    /// 每一路画面是哪个显示器（`HostMsg::Displays::shown`）
    fn shown(&self) -> Vec<u8> {
        self.view.iter().map(|v| v.display).collect()
    }

    fn displays_msg(&self) -> HostMsg {
        HostMsg::Displays {
            displays: self.displays.clone(),
            current: self.current(),
            shown: self.shown(),
        }
    }

    /// 改看 `view`（[`resolve_view`] 过的）：换了显示器的那几路停掉现在的编码管线、重新开始（新管线从关键帧开始，
    /// 旧的编码帧不能夹在中间），只是大小变了的更新画质，不再看的停掉。返回看的显示器变没变
    async fn apply_view(&mut self, view: Vec<StreamView>) -> bool {
        let old = std::mem::replace(&mut self.view, view);
        let moved = old.len() != self.view.len()
            || old
                .iter()
                .zip(&self.view)
                .any(|(a, b)| a.display != b.display);
        if moved {
            // 按着的鼠标键、按键不要带到另一个显示器上
            self.injector.release_all();
        }
        self.cursor.lock().unwrap().set_streams(self.view.len());
        for stream in 0..self.pipelines.len() {
            let (was, now) = (old.get(stream).copied(), self.view.get(stream).copied());
            let running = self.pipelines[stream].is_some();
            match (was, now) {
                (_, None) => {
                    if let Some(p) = self.pipelines[stream].take() {
                        p.stop().await;
                        // 不再看的一路：在途的帧不用等了（不然再看它时，很久以前没确认的帧会被当成画面停住了）
                        if let Some(f) = self.flows.get(stream as u8) {
                            f.stopped();
                        }
                    }
                }
                (Some(w), Some(n)) if running && w.display == n.display => {
                    if w.max_short != n.max_short
                        && let Some(p) = &self.pipelines[stream]
                    {
                        p.update_settings(self.stream_settings(stream));
                    }
                }
                (_, Some(n)) => {
                    if let Some(p) = self.pipelines[stream].take() {
                        p.stop().await;
                    }
                    self.pipelines[stream] = Some(Pipeline::start(pipeline::Config {
                        stream: stream as u8,
                        display: n.display,
                        settings: self.stream_settings(stream),
                        video: self.video[stream].clone(),
                        out: self.out.clone(),
                        flow: self
                            .flows
                            .get(stream as u8)
                            .expect("one per stream")
                            .clone(),
                        cursor: self.cursor.clone(),
                    }));
                }
            }
        }
        if moved {
            info!(session_id = self.session_id, view = ?self.view, "showing displays");
        }
        moved
    }

    /// 每一路画面对应的显示器（鼠标坐标按它换算）；列不出显示器时是一个占位的
    fn stream_displays(&self) -> Vec<DisplayInfo> {
        let ds: Vec<DisplayInfo> = self
            .view
            .iter()
            .filter_map(|v| self.displays.get(v.display as usize).cloned())
            .collect();
        if ds.is_empty() {
            vec![fallback_display()]
        } else {
            ds
        }
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

    /// 画面确认直接交给那一路画面的在途控制（读任务里调用，不等这里换显示器、重建管线）
    fn acks(&self) -> Arc<dyn Acks> {
        self.flows.clone()
    }

    /// 控制端麦克风的声音（读任务里调用）：打开了麦克风才交给播放线程
    fn mic(&self) -> Option<Arc<dyn MicSink>> {
        if mic::supported() {
            Some(self.mic.input.clone())
        } else {
            None
        }
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
        let view = resolve_view(&start.view, self.displays.len(), MAX_STREAMS);
        let (hostname, os) = crate::sysinfo::host_identity();
        info!(
            session_id = self.session_id,
            displays = self.displays.len(),
            ?view,
            "desktop session starting"
        );
        self.out.send(&HostMsg::Hello(HostHello {
            hostname,
            os,
            version: config::VERSION.to_string(),
            displays: self.displays.clone(),
            current_display: view.first().map_or(0, |v| v.display),
            codecs,
            platform: PLATFORM,
            // 桌面被控端只有鼠标键盘：不接收触摸，也没有手机那样的系统操作（快捷键由控制端发按键）
            touch: false,
            actions: Vec::new(),
            shown: view.iter().map(|v| v.display).collect(),
            // 每个显示器一路：采集、编码各是各的（硬件编码器同时开不了那么多时自己退回软件编码）
            streams: MAX_STREAMS,
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
        // 开始时的画质和看哪几个显示器在握手里：编码器一开始就按它建，不用先建一个默认的、等设置到了再重建
        self.settings = start.settings;
        self.apply_view(view).await;
        if start.audio {
            self.subscribe_audio()?;
        }
        let status = mic::probe(self.session_id).await;
        self.send_mic(status)?;
        Ok(())
    }

    /// 控制端确认了握手：是真的在连（重放的握手消息走不到这里），这时才在托盘里显示它
    fn confirmed(&mut self) {
        if let Some((gate, ip, admitted)) = self.identity.take() {
            gate.admitted(self.session_id, ip, &admitted);
        }
    }

    async fn request(&mut self, req: Request) -> Result<()> {
        match req {
            // 晚到太久的输入已经换成了"释放所有按键"。交给注入线程就返回：在这里等它注入完的话，
            // 这期间 Pong、画面的确认、本机用户的断开都处理不了（见 input::thread）
            Request::Input(events) => {
                let displays = self.stream_displays();
                self.injector.handle(events, &displays);
            }
            Request::Settings(s) => {
                self.settings = s;
                for (stream, p) in self.pipelines.iter().enumerate() {
                    if let Some(p) = p {
                        p.update_settings(self.stream_settings(stream));
                    }
                }
            }
            // 控制端要的：那一路；被控端自己要的（直连断了）：每一路
            Request::Keyframe(Some((stream, after))) => {
                if let Some(Some(p)) = self.pipelines.get(stream as usize) {
                    p.request_keyframe(Some(after));
                }
            }
            Request::Keyframe(None) => {
                for p in self.pipelines.iter().flatten() {
                    p.request_keyframe(None);
                }
            }
            Request::View(view) => {
                let view = resolve_view(&view, self.displays.len(), MAX_STREAMS);
                if view != self.view && self.apply_view(view).await {
                    self.out.send(&self.displays_msg())?;
                }
            }
            Request::Clipboard(text) => self.clip.apply(text),
            // 控制端复制了文件：放进本机剪贴板，粘贴时才取（见 crate::clipfiles）
            Request::ClipboardFiles(offer) => self.clip.apply_files(offer),
            Request::Audio(enabled) => {
                if !enabled {
                    self.sound = None;
                } else if self.sound.is_none() {
                    self.subscribe_audio()?;
                }
            }
            Request::Mic(enabled) => {
                self.mic.wanted = enabled;
                if enabled && self.mic.lease.is_none() {
                    self.open_mic()?;
                } else if !enabled {
                    // 放开设备，别的会话就能用了
                    self.mic.lease = None;
                    let status = mic::probe(self.session_id).await;
                    self.send_mic(status)?;
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
            status = mic_changed(&mut self.mic.lease) => Tick::Mic(status),
            // 只关心别的会话放开了麦克风：自己打开、放开的时候不在等
            _ = self.mic.owner.changed(), if self.mic.waiting() => Tick::MicCheck,
            _ = self.mic.tick.tick(), if self.mic.missing() => Tick::MicCheck,
        }
    }

    async fn event(&mut self, tick: Tick) -> Result<()> {
        let session_id = self.session_id;
        match tick {
            Tick::Clipboard => match self.clip.poll() {
                // 走大块通道：1 MiB 的剪贴板不挡控制消息。网络慢、前面排满了就丢掉这一次
                Some(Clip::Text(text)) => {
                    if let Some(f) = &self.files {
                        f.withdraw();
                    }
                    if let Err(e) = self.out.send(&HostMsg::Clipboard { text }) {
                        warn!(session_id, "dropped a clipboard update: {e:#}");
                    }
                }
                // 本机复制了文件：只发清单，控制端粘贴时才来取。
                Some(Clip::Files(paths)) => {
                    if let Some(f) = self.files.clone()
                        && let Some(provider) = super::clip_provider(paths).await
                    {
                        f.offer(provider);
                    }
                }
                None => {}
            },
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
                    let next = follow_view(&self.displays, &self.view, &now);
                    info!(session_id, ?now, view = ?next, "displays changed");
                    self.displays = now;
                    // 序号没变的那几路接着编：采集线程自己会重新打开（旋转、改分辨率……）
                    self.apply_view(next).await;
                    // 只留最新的一个（见 airhand_link::host::queue）
                    self.out.send(&self.displays_msg())?;
                }
            }
            Tick::Mic(status) => self.send_mic(status)?,
            // 能用了的话：控制端一直想开着就打开，不然告诉它现在能开了
            Tick::MicCheck => {
                if self.mic.wanted {
                    self.open_mic()?;
                } else {
                    let status = mic::probe(self.session_id).await;
                    self.send_mic(status)?;
                }
            }
        }
        Ok(())
    }

    /// 在看的任何一路画面很久没有确认了
    fn video_stalled(&self) -> bool {
        self.pipelines
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_some())
            .any(|(stream, _)| {
                self.flows
                    .get(stream as u8)
                    .is_some_and(|f| f.ack_stalled())
            })
    }

    async fn stop(mut self) {
        self.injector.cancel();
        // 先停掉还拿着画面 / 声音发送端的东西，写任务才能把排着队的（例如 Bye）发完
        for p in self.pipelines.iter_mut().filter_map(Option::take) {
            p.stop().await;
        }
        self.sound = None;
        self.mic.lease = None;
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

    fn view(displays: &[u8]) -> Vec<StreamView> {
        displays.iter().copied().map(StreamView::display).collect()
    }

    #[test]
    fn follows_the_displays_being_shown() {
        let old = [
            d(0, "DISPLAY1", 0, 1920, 1080),
            d(1, "DISPLAY2", 1920, 1920, 1080),
        ];
        // 转成竖屏：序号不变
        let rotated = [
            d(0, "DISPLAY1", 0, 1920, 1080),
            d(1, "DISPLAY2", 1920, 1080, 1920),
        ];
        assert_eq!(follow_view(&old, &view(&[1]), &rotated), view(&[1]));
        // 在左边插了一个显示器，正在看的那个排到了后面；两个都在看时两路各跟各的
        let plugged = [
            d(0, "DISPLAY1", 0, 1920, 1080),
            d(1, "DISPLAY3", 1920, 1280, 1024),
            d(2, "DISPLAY2", 3200, 1920, 1080),
        ];
        assert_eq!(follow_view(&old, &view(&[1]), &plugged), view(&[2]));
        assert_eq!(follow_view(&old, &view(&[0, 1]), &plugged), view(&[0, 2]));
        // 每一路的大小跟着它走
        let sized = [
            StreamView::display(0),
            StreamView {
                display: 1,
                max_short: 720,
            },
        ];
        assert_eq!(
            follow_view(&old, &sized, &plugged),
            [
                StreamView::display(0),
                StreamView {
                    display: 2,
                    max_short: 720
                }
            ]
        );
        // 正在看的被拔掉了：只看它的时候改看主显示器，还看着别的显示器就不要这一路了
        assert_eq!(follow_view(&old, &view(&[1]), &old[..1]), view(&[0]));
        assert_eq!(follow_view(&old, &view(&[1, 0]), &old[..1]), view(&[0]));
        // 序号本来就不对
        assert_eq!(follow_view(&old, &view(&[7]), &old), view(&[0]));
    }
}
