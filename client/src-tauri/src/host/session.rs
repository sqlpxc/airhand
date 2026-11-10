//! 被控端的单个远控会话。

use airhand_link::flow::Flow;
use airhand_link::host::{Admission, CursorCache, HostSession, Incoming, PEER_TIMEOUT};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use airhand_proto::session::{
    ControllerMsg, DisplayInfo, HostHello, HostMsg, Notice, Platform, QualitySettings,
};
use anyhow::Result;
use tracing::{info, warn};

use super::audio::{self, AudioSubscription};
use super::capture;
use super::gate::DesktopGate;
use super::input::Injector;
use super::pipeline::Pipeline;
use crate::clipboard::{self, ClipSync};
use crate::config;

/// 画面的发送队列：在途帧控制之下最多一两帧，多了宁可让编码器等着
const VIDEO_QUEUE: usize = 2;

/// 告诉控制端的平台（控制端据此选快捷键、提示的说法）
#[cfg(windows)]
const PLATFORM: Platform = Platform::Windows;
#[cfg(target_os = "macos")]
const PLATFORM: Platform = Platform::Mac;
#[cfg(not(any(windows, target_os = "macos")))]
const PLATFORM: Platform = Platform::Linux;
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

pub async fn run(inc: Incoming, gate: Arc<DesktopGate>) -> Result<()> {
    let session_id = inc.session_id;
    // 端到端握手：没配对过、又没有配对码的控制端，在这里就被拒绝了（见 gate）。
    // 返回的时候控制端已经确认过了（重放的握手消息走不到这里），这时才在托盘里显示它、开始采集
    let (mut io, admitted) = match HostSession::open(&inc, VIDEO_QUEUE, audio::QUEUE, &*gate).await {
        Ok(r) => r,
        Err(e) => {
            warn!(session_id, "controller not admitted: {e:#}");
            return Err(e);
        }
    };
    // 桌面被控端不请本机用户同意（无人值守），gate 不会要求它
    debug_assert_ne!(admitted.admission, Admission::NeedsApproval);
    gate.admitted(session_id, &admitted);
    let mut kick = inc.kick;
    let ctl = io.ctl.clone();
    let bulk = io.bulk.clone();

    let (mut displays, codecs, env_notice) = tokio::task::spawn_blocking(|| {
        (
            capture::displays(),
            super::encoder::available_codecs(),
            capture::environment_notice(),
        )
    })
    .await
    .unwrap_or_default();
    let mut current: u8 = 0;
    let (hostname, os) = crate::sysinfo::host_identity();
    info!(session_id, displays = displays.len(), "session started");

    ctl.send(&HostMsg::Hello(HostHello {
        hostname,
        os,
        version: config::VERSION.to_string(),
        displays: displays.clone(),
        current_display: current,
        codecs,
        platform: PLATFORM,
        // 桌面被控端只有鼠标键盘：不接收触摸，也没有手机那样的系统操作（快捷键由控制端发按键）
        touch: false,
        actions: Vec::new(),
    }))?;
    if displays.is_empty() {
        ctl.send(&notice(Notice::NoDisplays))?;
    }
    if let Some(n) = env_notice {
        ctl.send(&notice(n))?;
    }
    // 缺少系统权限（macOS）：告诉控制端为什么没有画面 / 不能操作；之后权限有变化时再发
    let mut missing = super::missing_permissions();
    if missing != (false, false) {
        ctl.send(&notice(permission_notice(missing)))?;
    }

    // 开始时的画质在握手里：编码器一开始就按它建，不用先建一个默认的、等设置到了再重建
    let mut settings = admitted.settings;
    let flow = Arc::new(Flow::default());
    // 控制端缓存了哪些光标形状：整个会话一份，换显示器（重建管线）时接着用
    let cursors = Arc::new(Mutex::new(CursorCache::default()));
    let start = |display: u8, settings: QualitySettings| {
        Pipeline::start(
            display,
            settings,
            io.video.clone(),
            ctl.clone(),
            bulk.clone(),
            flow.clone(),
            cursors.clone(),
        )
    };
    let mut pipeline = (!displays.is_empty()).then(|| start(current, settings));
    let mut injector = Injector::new();
    // 开始时要不要声音也在握手里
    let subscribe_audio = || -> Result<AudioSubscription> {
        let s = audio::subscribe(io.audio.clone());
        // 别的会话已经在采集、并且正打不开：马上告诉这个控制端
        if let Some(detail) = s.failure() {
            ctl.send(&notice(Notice::AudioFailed { detail }))?;
        }
        Ok(s)
    };
    let mut sound: Option<AudioSubscription> = if admitted.audio {
        Some(subscribe_audio()?)
    } else {
        None
    };
    let fallback = DisplayInfo {
        index: 0,
        name: String::new(),
        x: 0,
        y: 0,
        width: 1,
        height: 1,
        primary: true,
    };
    let mut clip = ClipSync::new();
    let mut clip_tick = tokio::time::interval(clipboard::POLL);
    clip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let mut permission_tick = tokio::time::interval(PERMISSION_CHECK);
    permission_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut display_tick = tokio::time::interval(DISPLAY_CHECK);
    display_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_msg = Instant::now();

    let result: Result<()> = async {
        loop {
            tokio::select! {
                msg = io.incoming.recv() => {
                    let Some(msg) = msg else { break };
                    last_msg = Instant::now();
                    match msg {
                        // 晚到太久的输入已经在 HostSession 里换成了"释放所有按键"
                        ControllerMsg::Input { events, .. } => {
                            let d = displays.get(current as usize).unwrap_or(&fallback);
                            injector.handle(&events, d);
                        }
                        ControllerMsg::Ack { seq } => flow.ack(seq),
                        ControllerMsg::Settings(s) => {
                            settings = s.sanitized();
                            if let Some(p) = &pipeline {
                                p.update_settings(settings);
                            }
                        }
                        ControllerMsg::RequestKeyframe => {
                            if let Some(p) = &pipeline {
                                p.request_keyframe();
                            }
                        }
                        ControllerMsg::SelectDisplay { index } => {
                            if (index as usize) < displays.len() && index != current {
                                current = index;
                                injector.release_all();
                                // 先停旧管线再启动新管线（新管线从关键帧开始）
                                if let Some(p) = pipeline.take() { p.stop().await; }
                                pipeline = Some(start(current, settings));
                                ctl.send(&HostMsg::Displays { displays: displays.clone(), current })?;
                            }
                        }
                        ControllerMsg::Ping { t } => ctl.send(&HostMsg::Pong { t })?,
                        ControllerMsg::Clipboard { text } => clip.apply(text),
                        ControllerMsg::Audio { enabled } => {
                            if !enabled {
                                sound = None;
                            } else if sound.is_none() {
                                sound = Some(subscribe_audio()?);
                            }
                        }
                    }
                }
                _ = clip_tick.tick() => {
                    // 走大块通道：1 MiB 的剪贴板不挡控制消息。网络慢、前面排满了就丢掉这一次
                    if let Some(text) = clip.poll().await
                        && let Err(e) = bulk.send(&HostMsg::Clipboard { text })
                    {
                        warn!(session_id, "dropped a clipboard update: {e:#}");
                    }
                }
                _ = kick.changed() => {
                    let reason = *kick.borrow_and_update();
                    info!(session_id, ?reason, "the local user disconnected the controller");
                    ctl.send(&HostMsg::Bye(reason))?;
                    break;
                }
                e = io.writer.failed() => return Err(e),
                _ = watchdog.tick() => {
                    if last_msg.elapsed() > PEER_TIMEOUT {
                        anyhow::bail!("the controller stopped responding");
                    }
                }
                // 声音采集打不开 / 恢复了（只在控制端开着声音时）
                failure = audio_failure(&mut sound) => {
                    ctl.send(&notice(match failure {
                        Some(detail) => Notice::AudioFailed { detail },
                        None => Notice::AudioRecovered,
                    }))?;
                }
                // 例如本机用户在会话中途打开了"辅助功能"：更新（两项都授予时撤下）控制端的提示
                _ = permission_tick.tick() => {
                    let now = super::missing_permissions();
                    if now != missing {
                        info!(session_id, screen = !now.0, input = !now.1, "permissions changed");
                        missing = now;
                        ctl.send(&notice(permission_notice(now)))?;
                    }
                }
                // 显示器变了：鼠标坐标是按这里的列表换算的，旧列表会让点击落到别的位置
                // （例如把显示器转成竖屏后宽高对调）。更新列表并告诉控制端；采集线程自己会重新打开。
                // 正在看的显示器换了序号就跟着它走，被拔掉了就改看主显示器
                _ = display_tick.tick() => {
                    let now = tokio::task::spawn_blocking(capture::displays).await.unwrap_or_default();
                    // 列不出来（例如切换显示模式的瞬间）时保留原来的列表
                    if !now.is_empty() && now != displays {
                        let next = follow_display(&displays, current, &now);
                        info!(session_id, ?now, current = next, "displays changed");
                        displays = now;
                        if next != current || pipeline.is_none() {
                            current = next;
                            injector.release_all();
                            if let Some(p) = pipeline.take() { p.stop().await; }
                            pipeline = Some(start(current, settings));
                        }
                        ctl.send(&HostMsg::Displays { displays: displays.clone(), current })?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    injector.release_all();
    // 先停掉还拿着画面 / 声音发送端的东西，写任务才能把排着队的（例如 Bye）发完
    if let Some(p) = pipeline {
        p.stop().await;
    }
    drop(sound);
    drop((ctl, bulk));
    io.close(session_id, result).await
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
