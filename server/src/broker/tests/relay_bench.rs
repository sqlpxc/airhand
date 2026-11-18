use super::*;
use airhand_proto::SessionHeader;

/// Runs one frame at a time to measure forwarding latency without a sender backlog.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run in release mode to measure loopback relay latency"]
async fn relay_video_bench() {
    const WARMUP: usize = 64;
    const SAMPLES: usize = 768;
    let (_broker, cfg) = start().await;
    let (mut host, id) = register(&cfg, None).await;
    let mut controller = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let sid = ask(&mut controller, 1, id.code, None)
        .await
        .unwrap()
        .session_id;
    assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));

    for size in [16 * 1024, 256 * 1024] {
        let data = vec![0x5a; size];
        let mut times = Vec::with_capacity(SAMPLES);
        for sample in 0..WARMUP + SAMPLES {
            let start = Instant::now();
            let body = timeout(Duration::from_secs(2), async {
                let mut send = open_lane(&host.conn, sid, SessionLane::Video(0))
                    .await
                    .unwrap();
                send.write_all(&data).await.unwrap();
                send.finish().unwrap();
                controller
                    .conn
                    .accept_uni()
                    .await
                    .unwrap()
                    .read_to_end(SessionHeader::LEN + size)
                    .await
                    .unwrap()
            })
            .await
            .unwrap();
            let elapsed = start.elapsed();
            assert_eq!(body.len(), SessionHeader::LEN + size);
            if sample >= WARMUP {
                times.push(elapsed);
            }
        }
        times.sort_unstable();
        println!(
            "{size} B/frame, {SAMPLES} samples: p50={} us, p95={} us",
            times[SAMPLES / 2].as_micros(),
            times[SAMPLES * 95 / 100].as_micros()
        );
    }
}
