//! 编码的基准：采集第一个显示器 3 秒，用各种编码器、分辨率、画面来路（GPU 零拷贝 / CPU 缩放）编码同一批画面。
//! `cargo run --release --example encode_bench`
//!
//! 每帧都当作新画面（重新读回内存），和画面一直在变时一样。时间是交给编码器到拿到码流（同步的），
//! 包括 CPU 路径上的读回、缩放、色彩转换。
use airhand_lib::host::{capture, encoder::Encoder};
use airhand_proto::session::{Codec, QualitySettings, VideoPriority};
use std::time::{Duration, Instant};
fn main() {
    let sub = capture::subscribe(0, 60, capture::Output::Native);
    sub.slot().wait_change(0, u64::MAX, Duration::from_secs(3));
    let (_, f) = sub.slot().try_newer(0).expect("capture");
    println!(
        "screen {}x{} ({})",
        f.width,
        f.height,
        if f.gpu.is_some() {
            "on the GPU"
        } else {
            "in memory"
        }
    );
    let mut frames = vec![f.clone()];
    let mut seq = 0;
    let t = Instant::now();
    while frames.len() < 30 && t.elapsed() < Duration::from_secs(3) {
        sub.slot()
            .wait_change(seq, u64::MAX, Duration::from_millis(50));
        if let Some((s, f)) = sub.slot().try_newer(seq) {
            seq = s;
            frames.push(f);
        } else {
            frames.push(frames.last().unwrap().clone());
        }
    }
    use VideoPriority::{Clarity, Latency};
    for (codec, hw, max_short, priority) in [
        (Codec::H264, false, 1080, Clarity),
        (Codec::H264, true, 1080, Clarity),
        (Codec::H264, true, 1080, Latency),
        (Codec::H265, true, 1080, Clarity),
        (Codec::H265, true, 1080, Latency),
        (Codec::AV1, true, 1080, Clarity),
        (Codec::AV1, true, 1080, Latency),
        (Codec::H264, true, 0, Clarity),
        (Codec::H264, true, 0, Latency),
        (Codec::AV1, true, 0, Latency),
    ] {
        let s = QualitySettings {
            codec,
            max_short,
            fps: 60,
            priority,
            ..Default::default()
        };
        let mut e = Encoder::new(&f, &s, 6000, hw, true).unwrap();
        let first = frames[0].uncached();
        let t = Instant::now();
        let k = e.encode(&first, true).unwrap().unwrap();
        let key_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut total = 0;
        let mut tt = Vec::new();
        let mut n = 0;
        for fr in &frames[1..] {
            let fr = fr.uncached();
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
