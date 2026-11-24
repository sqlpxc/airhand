//! macOS 系统声音采集：ScreenCaptureKit 的声音输出（macOS 13+），录下系统正在播放的声音（混音之后），
//! 不包括麦克风，也不包括本进程自己播放的声音。
//!
//! 直接让系统输出 48 kHz 立体声 float，不需要自己重采样。ScreenCaptureKit 必须指定一个显示器，
//! 这里用主显示器，画面按最小尺寸、最低帧率输出并直接丢掉。一个显示器都没有（合着盖子，补屏的虚拟显示器还没出来）时
//! 打不开，报 [`super::NoDisplay`]：不当作采集失败，过一会儿再试。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use airhand_proto::media::AUDIO_SAMPLE_RATE;
use anyhow::{Result, anyhow, bail};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_screen_capture_kit::{SCStreamConfiguration, SCStreamOutputType};

use crate::host::{macos, sck};

/// `kAudioFormatFlagIsFloat`
const FLAG_FLOAT: u32 = 1 << 0;
/// `kAudioFormatFlagIsNonInterleaved`
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;
/// 采集线程跟不上时最多攒这么多（1 秒），再多就丢掉最旧的
const MAX_BUFFERED: usize = AUDIO_SAMPLE_RATE as usize * 2;

#[derive(Default)]
struct Inbox {
    /// 48 kHz 立体声交错 float
    pcm: Vec<f32>,
    /// `pcm[0]` 的采集时间
    first: Option<Instant>,
    stopped: Option<String>,
    /// 格式不对时只记一次日志
    format_warned: bool,
}

pub struct Loopback {
    _stream: sck::Stream,
    inbox: Arc<Mutex<Inbox>>,
}

impl Loopback {
    pub fn open() -> Result<Self> {
        macos::check_screen_permission()?;
        let display = macos::displays()
            .into_iter()
            .next()
            .ok_or(super::NoDisplay)?;
        let target = sck::shareable_display(display.id, &[])?;
        let config = unsafe {
            let c = SCStreamConfiguration::new();
            c.setCapturesAudio(true);
            c.setSampleRate(AUDIO_SAMPLE_RATE as isize);
            c.setChannelCount(2);
            c.setExcludesCurrentProcessAudio(true);
            // 画面用不到：最小尺寸、每秒一帧
            c.setWidth(2);
            c.setHeight(2);
            c.setShowsCursor(false);
            c.setMinimumFrameInterval(CMTime::new(1, 1));
            c.setQueueDepth(3);
            c
        };
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let (rx, stop) = (inbox.clone(), inbox.clone());
        // 也要接收画面样本（直接丢掉），否则系统会不停地在日志里抱怨找不到画面的接收者
        let stream = sck::Stream::start(
            &target,
            &config,
            &[SCStreamOutputType::Screen, SCStreamOutputType::Audio],
            "app.airhand.audio",
            move |sb, kind| {
                if kind == SCStreamOutputType::Audio {
                    receive(sb, &rx);
                }
            },
            move |e| stop.lock().unwrap().stopped = Some(e),
        )?;
        tracing::info!("audio capture opened (ScreenCaptureKit)");
        Ok(Self {
            _stream: stream,
            inbox,
        })
    }

    /// 读出当前已采集的全部数据（48 kHz 立体声交错 float），追加到 `out`。没有数据时立即返回。
    ///
    /// 返回追加的第一个采样的采集时间。
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<Option<Instant>> {
        let mut inbox = self.inbox.lock().unwrap();
        if let Some(e) = inbox.stopped.take() {
            bail!("the system stopped the audio capture ({e})");
        }
        out.append(&mut inbox.pcm);
        Ok(inbox.first.take())
    }

    /// ScreenCaptureKit 录的是系统混音，与输出设备无关，换设备不需要重新打开。
    pub fn default_changed(&mut self) -> bool {
        false
    }
}

/// 在 ScreenCaptureKit 的队列上调用：把一包声音转换成交错的立体声，放进收件箱。
fn receive(sb: &CMSampleBuffer, inbox: &Mutex<Inbox>) {
    let captured = macos::instant_from_host_time(unsafe { sb.presentation_time_stamp() });
    let mut inbox = inbox.lock().unwrap();
    let before = inbox.pcm.len();
    match append_stereo(sb, &mut inbox.pcm) {
        Ok(()) => {
            if before == 0 && !inbox.pcm.is_empty() {
                inbox.first = captured.or_else(|| Some(Instant::now()));
            }
            let len = inbox.pcm.len();
            if len > MAX_BUFFERED {
                // 丢掉最旧的数据（整帧丢，保持左右声道对齐），时间戳顺延
                let drop = (len - MAX_BUFFERED) & !1;
                inbox.pcm.drain(..drop);
                if let Some(t) = inbox.first.as_mut() {
                    *t += std::time::Duration::from_secs_f64(
                        drop as f64 / 2.0 / AUDIO_SAMPLE_RATE as f64,
                    );
                }
            }
        }
        Err(e) => {
            if !inbox.format_warned {
                inbox.format_warned = true;
                tracing::warn!("can't read the captured audio: {e:#}");
            }
        }
    }
}

/// 一包声音 → 48 kHz 立体声交错 float，追加到 `out`。
fn append_stereo(sb: &CMSampleBuffer, out: &mut Vec<f32>) -> Result<()> {
    let desc = unsafe { sb.format_description() }.ok_or_else(|| anyhow!("no audio format"))?;
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&desc).as_ref() }
        .ok_or_else(|| anyhow!("not an audio format"))?;
    if asbd.mFormatFlags & FLAG_FLOAT == 0 || asbd.mBitsPerChannel != 32 {
        bail!("unsupported sample format (flags {:#x})", asbd.mFormatFlags);
    }
    if asbd.mSampleRate as u32 != AUDIO_SAMPLE_RATE {
        bail!("unexpected sample rate {}", asbd.mSampleRate);
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let planar = asbd.mFormatFlags & FLAG_NON_INTERLEAVED != 0;

    // 先问需要多大的 AudioBufferList（非交错时每个声道一个缓冲）
    let mut size = 0usize;
    unsafe {
        sb.audio_buffer_list_with_retained_block_buffer(
            &mut size,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            std::ptr::null_mut(),
        )
    };
    let words = size.max(size_of::<AudioBufferList>()).div_ceil(8);
    let mut storage = vec![0u64; words];
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = unsafe {
        sb.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            list,
            words * 8,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block,
        )
    };
    if status != 0 {
        bail!("CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer: {status}");
    }
    // 数据在 block 里，用完之前不能释放
    let _block = std::ptr::NonNull::new(block).map(|b| unsafe { CFRetained::from_raw(b) });
    let n = unsafe { (*list).mNumberBuffers } as usize;
    let buffers: &[AudioBuffer] =
        unsafe { std::slice::from_raw_parts((&raw const (*list).mBuffers).cast(), n) };
    let plane = |b: &AudioBuffer| -> &[f32] {
        if b.mData.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(b.mData.cast(), b.mDataByteSize as usize / 4) }
        }
    };
    if planar {
        let Some(l) = buffers.first() else {
            return Ok(());
        };
        // 单声道时左右声道相同
        let (l, r) = (plane(l), plane(buffers.get(1).unwrap_or(l)));
        let frames = l.len().min(r.len());
        out.reserve(frames * 2);
        for i in 0..frames {
            out.extend_from_slice(&[l[i], r[i]]);
        }
    } else if let Some(b) = buffers.first() {
        let data = plane(b);
        let ch = (b.mNumberChannels as usize).max(1).min(channels.max(1));
        out.reserve(data.len() / ch * 2);
        for f in data.chunks_exact(ch) {
            out.extend_from_slice(&[f[0], f[ch.min(2) - 1]]);
        }
    }
    Ok(())
}

// ───────────────────────────── 测试 ─────────────────────────────

/// 测试用：在默认输出设备上播放一个很轻的正弦波（AudioQueue），Drop 时停止。
#[cfg(test)]
pub(crate) struct TonePlayer {
    queue: *mut std::ffi::c_void,
    _state: Box<tone::State>,
}

#[cfg(test)]
unsafe impl Send for TonePlayer {}

#[cfg(test)]
mod tone {
    use std::ffi::c_void;

    use objc2_core_audio_types::AudioStreamBasicDescription;

    #[repr(C)]
    pub struct AudioQueueBuffer {
        pub capacity: u32,
        pub data: *mut c_void,
        pub size: u32,
        pub user: *mut c_void,
        pub packet_capacity: u32,
        pub packets: *mut c_void,
        pub packet_count: u32,
    }

    pub type Callback = extern "C-unwind" fn(*mut c_void, *mut c_void, *mut AudioQueueBuffer);

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C-unwind" {
        pub fn AudioQueueNewOutput(
            format: *const AudioStreamBasicDescription,
            callback: Callback,
            user: *mut c_void,
            run_loop: *const c_void,
            mode: *const c_void,
            flags: u32,
            out: *mut *mut c_void,
        ) -> i32;
        pub fn AudioQueueAllocateBuffer(
            queue: *mut c_void,
            size: u32,
            out: *mut *mut AudioQueueBuffer,
        ) -> i32;
        pub fn AudioQueueEnqueueBuffer(
            queue: *mut c_void,
            buffer: *mut AudioQueueBuffer,
            packets: u32,
            descs: *const c_void,
        ) -> i32;
        pub fn AudioQueueStart(queue: *mut c_void, at: *const c_void) -> i32;
        pub fn AudioQueueDispose(queue: *mut c_void, immediate: u8) -> i32;
    }

    pub struct State {
        pub phase: f64,
        pub step: f64,
        pub amp: f32,
    }

    /// AudioQueue 的内部线程上调用：填满缓冲并重新排队
    pub extern "C-unwind" fn fill(
        user: *mut c_void,
        queue: *mut c_void,
        buf: *mut AudioQueueBuffer,
    ) {
        unsafe {
            let st = &mut *(user as *mut State);
            let b = &mut *buf;
            let frames = b.capacity as usize / 8;
            let out = std::slice::from_raw_parts_mut(b.data as *mut f32, frames * 2);
            for f in out.as_chunks_mut::<2>().0 {
                let v = st.phase.sin() as f32 * st.amp;
                st.phase += st.step;
                f[0] = v;
                f[1] = v;
            }
            b.size = (frames * 8) as u32;
            AudioQueueEnqueueBuffer(queue, buf, 0, std::ptr::null());
        }
    }
}

#[cfg(test)]
impl TonePlayer {
    pub fn start(freq: f64, amp: f32) -> Result<Self> {
        use objc2_core_audio_types::{AudioStreamBasicDescription, kAudioFormatLinearPCM};
        let format = AudioStreamBasicDescription {
            mSampleRate: AUDIO_SAMPLE_RATE as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: FLAG_FLOAT | (1 << 3), // float、紧密排列
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let mut state = Box::new(tone::State {
            phase: 0.0,
            step: 2.0 * std::f64::consts::PI * freq / AUDIO_SAMPLE_RATE as f64,
            amp,
        });
        let mut queue = std::ptr::null_mut();
        unsafe {
            let st = tone::AudioQueueNewOutput(
                &format,
                tone::fill,
                (&raw mut *state).cast(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut queue,
            );
            if st != 0 {
                bail!("AudioQueueNewOutput: {st}");
            }
            // 3 个 20 ms 的缓冲
            for _ in 0..3 {
                let mut buf = std::ptr::null_mut();
                if tone::AudioQueueAllocateBuffer(queue, AUDIO_SAMPLE_RATE / 50 * 8, &mut buf) == 0
                {
                    tone::fill((&raw mut *state).cast(), queue, buf);
                }
            }
            let st = tone::AudioQueueStart(queue, std::ptr::null());
            if st != 0 {
                tone::AudioQueueDispose(queue, 1);
                bail!("AudioQueueStart: {st}");
            }
        }
        Ok(Self {
            queue,
            _state: state,
        })
    }
}

#[cfg(test)]
impl Drop for TonePlayer {
    fn drop(&mut self) {
        // 立即停止并等内部线程退出，之后才能释放 state
        unsafe { tone::AudioQueueDispose(self.queue, 1) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 需要"屏幕与系统录音"权限和一个输出设备。会以 -60 dBFS（几乎听不见）播放 1.5 秒 440 Hz。
    /// `cargo test --lib loopback_captures_tone -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn loopback_captures_tone() {
        if !sck::require_permission() {
            return;
        }
        let _tone = TonePlayer::start(440.0, 0.001).expect("播放测试音");
        let mut cap = Loopback::open().expect("打开声音采集");
        let mut pcm = Vec::new();
        let started = Instant::now();
        let mut stamps = 0;
        while started.elapsed() < Duration::from_millis(1500) {
            if let Some(t) = cap.read(&mut pcm).unwrap() {
                assert!(t.elapsed() < Duration::from_millis(500), "时间戳过旧");
                stamps += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let secs = pcm.len() as f64 / 2.0 / AUDIO_SAMPLE_RATE as f64;
        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!("采集 {secs:.2} 秒（{stamps} 次带时间戳），峰值 {peak:.2e}");
        assert!(secs > 1.0, "采集到的数据太少: {secs:.2} 秒");
        assert!(peak > 1e-4, "没有采集到测试音");
    }

    /// 只播放（不需要任何权限），确认测试音本身能工作
    #[test]
    fn tone_player_starts() {
        match TonePlayer::start(440.0, 0.0) {
            Ok(t) => {
                std::thread::sleep(Duration::from_millis(100));
                drop(t);
            }
            Err(e) => println!("没有声音输出设备？{e:#}"),
        }
    }
}
