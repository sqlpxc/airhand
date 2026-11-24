//! ScreenCaptureKit 流（macOS 12.3+，声音 13+）：画面采集（`capture/sck.rs`）和声音采集（`audio/sck.rs`）共用。
//!
//! ScreenCaptureKit 是异步 API：样本在我们创建的串行队列上回调。这里把它包装成
//! "启动（等待完成）→ 回调交给闭包 → Drop 时停止（等待完成）"，调用方各自把样本放进自己的收件箱，
//! 由采集线程按需取用。

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{NSArray, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};

/// ScreenCaptureKit 的异步调用最多等这么久
const TIMEOUT: Duration = Duration::from_secs(5);

/// 把不是 `Send` 的对象交给另一个线程（只在交接时用，之后只有一个线程访问）。
struct Handoff<T>(T);
unsafe impl<T> Send for Handoff<T> {}

/// 已经 retain 过的样本，可以交给采集线程处理（CMSampleBuffer 的引用计数是线程安全的）。
pub struct Sample(pub CFRetained<CMSampleBuffer>);
unsafe impl Send for Sample {}

impl Sample {
    pub fn retain(sb: &CMSampleBuffer) -> Self {
        Self(unsafe { CFRetained::retain(NonNull::from(sb)) })
    }
}

/// 错误的英文说明：只有错误域和错误码，不含操作系统按本机语言给出的文字（会发给控制端，见 `wire_detail`）。
/// 本地化的说明只写进日志。
pub fn error_code(e: &NSError) -> String {
    tracing::debug!("ScreenCaptureKit error: {}", e.localizedDescription());
    format!("{} {}", e.domain(), e.code())
}

type OnSample = Box<dyn Fn(&CMSampleBuffer, SCStreamOutputType) + Send + Sync>;
type OnStop = Box<dyn Fn(String) + Send + Sync>;

pub struct Handlers {
    on_sample: OnSample,
    on_stop: OnStop,
}

define_class!(
    // SAFETY: NSObject 没有特殊的子类化要求；本类没有实现 Drop（实例变量会被正常释放）
    #[unsafe(super(NSObject))]
    #[name = "AirhandStreamOutput"]
    #[ivars = Handlers]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            (self.ivars().on_sample)(sample, kind);
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            (self.ivars().on_stop)(error_code(error));
        }
    }
);

impl StreamOutput {
    fn new(handlers: Handlers) -> Retained<Self> {
        let this = Self::alloc().set_ivars(handlers);
        unsafe { msg_send![super(this), init] }
    }
}

/// 要采集的显示器（ScreenCaptureKit 里的对象）和采集时要去掉的窗口
pub struct Target {
    display: Retained<SCDisplay>,
    excluded: Retained<NSArray<SCWindow>>,
}

/// ScreenCaptureKit 里与某个 CoreGraphics 显示器对应的对象。`exclude`：采集时要去掉的窗口（窗口编号，
/// 防窥模式的黑窗口），都得在 ScreenCaptureKit 列出的窗口里（这时连不在屏幕上的窗口也列出来：睡着的显示器上的
/// 也要找得到）；还没列出来就报错，过一会儿再来
pub fn shareable_display(id: u32, exclude: &[u32]) -> Result<Target> {
    let (tx, rx) = mpsc::channel::<Result<Handoff<Retained<SCShareableContent>>, String>>();
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let r = match unsafe { Retained::retain(content) } {
                Some(c) => Ok(Handoff(c)),
                None => Err(unsafe { error.as_ref() }
                    .map(error_code)
                    .unwrap_or_else(|| "unknown error".into())),
            };
            let _ = tx.send(r);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false,
            exclude.is_empty(),
            &block,
        );
    }
    let content = rx
        .recv_timeout(TIMEOUT)
        .map_err(|_| anyhow!("ScreenCaptureKit didn't respond in time"))?
        .map_err(|e| anyhow!("can't list the screens (SCShareableContent: {e})"))?
        .0;
    let displays = unsafe { content.displays() };
    let display = (0..displays.count())
        .map(|i| displays.objectAtIndex(i))
        .find(|d| unsafe { d.displayID() } == id)
        .ok_or_else(|| anyhow!("ScreenCaptureKit doesn't know display {id}"))?;
    let mut excluded = Vec::new();
    if !exclude.is_empty() {
        let windows = unsafe { content.windows() };
        excluded.extend(
            (0..windows.count())
                .map(|i| windows.objectAtIndex(i))
                .filter(|w| exclude.contains(&unsafe { w.windowID() })),
        );
        if excluded.len() < exclude.len() {
            return Err(anyhow!(
                "ScreenCaptureKit doesn't list the privacy mode's black windows yet"
            ));
        }
    }
    Ok(Target {
        display,
        excluded: NSArray::from_retained_slice(&excluded),
    })
}

/// 一个正在运行的采集流，Drop 时停止。
pub struct Stream {
    stream: Retained<SCStream>,
    // 流只弱引用代理对象，需要自己持有
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

impl Stream {
    /// 采集整个显示器（去掉 `target` 说的那几个窗口），`kinds` 里的每种样本都交给 `on_sample`（在内部的串行
    /// 队列上调用）；流被系统停止（例如显示器拔掉、权限被收回）时调用 `on_stop`。
    pub fn start(
        target: &Target,
        config: &SCStreamConfiguration,
        kinds: &[SCStreamOutputType],
        label: &str,
        on_sample: impl Fn(&CMSampleBuffer, SCStreamOutputType) + Send + Sync + 'static,
        on_stop: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self> {
        let output = StreamOutput::new(Handlers {
            on_sample: Box::new(on_sample),
            on_stop: Box::new(on_stop),
        });
        let queue = DispatchQueue::new(label, None);
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &target.display,
                &target.excluded,
            );
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        for &kind in kinds {
            unsafe {
                stream.addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    kind,
                    Some(&queue),
                )
            }
            .map_err(|e| anyhow!("SCStream addStreamOutput failed: {}", error_code(&e)))?;
        }
        wait(|done| unsafe { stream.startCaptureWithCompletionHandler(Some(done)) })
            .map_err(|e| anyhow!("can't start ScreenCaptureKit capture ({e})"))?;
        Ok(Self {
            stream,
            _output: output,
            _queue: queue,
        })
    }

    /// 修改配置（例如帧率），不中断采集。
    pub fn update(&self, config: &SCStreamConfiguration) {
        unsafe {
            self.stream
                .updateConfiguration_completionHandler(config, None)
        };
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let stream = &self.stream;
        if let Err(e) = wait(|done| unsafe { stream.stopCaptureWithCompletionHandler(Some(done)) })
        {
            tracing::debug!("stopping ScreenCaptureKit capture: {e}");
        }
    }
}

/// 调用一个带完成回调的异步方法并等待结果。
fn wait(call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let block = RcBlock::new(move |e: *mut NSError| {
        let _ = tx.send(unsafe { e.as_ref() }.map(error_code));
    });
    call(&block);
    match rx.recv_timeout(TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e),
        Err(_) => Err("timed out".into()),
    }
}

/// 画面样本的状态（`SCFrameStatus`）：只有 `Complete` 的样本带有新画面，`Idle` 表示画面没有变化。
pub fn frame_status(sb: &CMSampleBuffer) -> Option<isize> {
    use objc2_foundation::NSDictionary;
    let array = unsafe { sb.sample_attachments_array(false) }?;
    // CFArray / CFDictionary 与 NSArray / NSDictionary 是"免费桥接"的同一种对象
    let array: &NSArray<NSDictionary<NSString, NSNumber>> =
        unsafe { &*CFRetained::as_ptr(&array).as_ptr().cast() };
    if array.count() == 0 {
        return None;
    }
    let info = array.objectAtIndex(0);
    let key: &NSString = unsafe { objc2_screen_capture_kit::SCStreamFrameInfoStatus };
    info.objectForKey(key).map(|n| n.integerValue())
}

// SCStream 本身是线程安全的（启动、停止、更新配置都可以在任意线程调用）
unsafe impl Send for Stream {}

#[cfg(test)]
pub(crate) fn require_permission() -> bool {
    let ok = crate::host::macos::screen_allowed();
    if !ok {
        println!("没有屏幕录制权限，跳过（系统设置 → 隐私与安全性 → 屏幕与系统录音）");
    }
    ok
}
