//! 编码的基准：采集第一个显示器，测量各种编码器、分辨率下的每帧编码时间。
//! `cargo run --release --example encode_bench`
//!
//! 每帧都当作新画面（重新读回内存），和画面一直在变时一样。时间是交给编码器到拿到码流（同步的），
//! 包括 CPU 路径上的读回、缩放、色彩转换。
use airhand_lib::host::{capture, encoder::Encoder};
use airhand_proto::session::{Codec, QualitySettings};
use std::time::{Duration, Instant};

fn main() {
    // 和正式的被控端一样：双显卡电脑上桌面复制要找对显卡，在用到 DXGI 之前
    #[cfg(windows)]
    capture::ignore_gpu_preference();
    for (codec, hw, max_short) in [
        (Codec::H264, false, 1080),
        (Codec::H264, true, 1080),
        (Codec::H265, true, 1080),
        (Codec::AV1, true, 1080),
        (Codec::H264, true, 0),
        (Codec::AV1, true, 0),
    ] {
        let sub = capture::subscribe(0, 60, max_short, false);
        sub.slot()
            .wait_change(0, u64::MAX, Duration::from_secs(3), false);
        let (mut seq, mut f) = sub.slot().try_newer(0, false).expect("capture");
        let s = QualitySettings {
            codec,
            max_short,
            fps: 60,
            ..Default::default()
        };
        let mut e = Encoder::new(&f, &s, 6000, hw, true).unwrap();
        let first = f.uncached();
        let t = Instant::now();
        let k = e.encode(&first, true).unwrap().unwrap();
        let key_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut total = 0;
        let mut tt = Vec::new();
        let mut n = 0;
        // 系统的帧缓冲池有限（ScreenCaptureKit）：逐帧取样，不持有整批 GPU 帧
        for _ in 1..30 {
            sub.slot()
                .wait_change(seq, u64::MAX, Duration::from_millis(50), false);
            if let Some(newer) = sub.slot().try_newer(seq, false) {
                (seq, f) = newer;
            }
            let fr = f.uncached();
            let t = Instant::now();
            if let Some(o) = e.encode(&fr, false).unwrap() {
                total += o.data.len();
                n += 1;
            }
            tt.push(t.elapsed().as_secs_f64() * 1e3);
        }
        tt.sort_by(f64::total_cmp);
        let avg = tt.iter().sum::<f64>() / tt.len().max(1) as f64;
        let pct = |p: usize| tt.get(tt.len() * p / 100).copied().unwrap_or_default();
        let i = e.info();
        println!(
            "{:30} {:4} {}x{} key {:>7}B {:>6.1}ms | P n={n} avg {:>6}B {:>5.2}ms p50 {:>5.2}ms p90 {:>5.2}ms",
            i.name,
            if e.uses_gpu() { "GPU" } else { "CPU" },
            i.width,
            i.height,
            k.data.len(),
            key_ms,
            total / n.max(1),
            avg,
            pct(50),
            pct(90),
        );
    }
}
