//! Real relay, Noise negotiation, extended packets and frontend feedback.
use super::*;
use airhand_link::controller::Command;
use airhand_link::video::VideoSession;
use airhand_proto::media::{self, VideoHeader};
use airhand_proto::session::{Codec, ControllerMsg, HostMsg};
use airhand_proto::video::*;

struct Ui {
    packets: mpsc::UnboundedSender<Vec<u8>>,
    configs: mpsc::UnboundedSender<VideoConfig>,
}
impl SessionUi for Ui {
    fn packet(&self, packet: Vec<u8>) -> Result<(), UiClosed> {
        self.packets.send(packet).map_err(|_| UiClosed)
    }
    fn event(&self, event: SessionEvent) -> Result<(), UiClosed> {
        if let SessionEvent::VideoConfig { config } = event {
            self.configs.send(config).map_err(|_| UiClosed)?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn video_contract_and_feedback_cross_the_real_relay() {
    let (_broker, cfg) = start().await;
    let (mut host, id) = register(&cfg, None).await;
    let receiver = ReceiverCapabilities {
        codecs: vec![DecodeCapability {
            codec: Codec::H264,
            max_width: Some(1280),
            max_height: Some(720),
            max_fps: Some(30),
        }],
    };
    let expected_receiver = receiver.clone();
    let host_task = tokio::spawn(async move {
        let ServerMsg::Incoming { session_id, peer } = next(&mut host).await else {
            panic!()
        };
        let (_kick, kick) = watch::channel(ByeReason::Kicked);
        let inc = Incoming {
            conn: host.conn.clone(),
            session_id,
            code: id.code,
            peer,
            kick,
            approvals: Default::default(),
        };
        let gate = OpenGate(Identity::generate());
        let (mut io, admitted) = HostStreams::open(&inc, 2, 16, &gate).await.unwrap();
        assert!(
            admitted
                .negotiated
                .has(airhand_proto::features::session::VIDEO_MODEL)
        );
        assert_eq!(admitted.video.as_ref().unwrap().receiver, expected_receiver);
        let model = VideoSession::new(admitted.video, io.out.clone());
        let selected = model
            .select(admitted.settings, &[Codec::H264, Codec::AV1], 1920, 1080)
            .unwrap();
        assert_eq!(
            (
                selected.settings.codec,
                selected.settings.fps,
                selected.settings.max_short
            ),
            (Codec::H264, 30, 720)
        );
        let h = VideoHeader {
            seq: 1,
            key: true,
            codec: Codec::H264,
            width: 1280,
            height: 720,
            capture_us: 5000,
        };
        let header = model.header(&h, selected, 2000).unwrap();
        io.video
            .send(media::frame(&header, &[b"encoded frame"]).into())
            .await
            .unwrap();
        let mut got_feedback = false;
        let mut got_ack = false;
        let mut got_view = false;
        while !(got_feedback && got_ack && got_view) {
            match timeout(Duration::from_secs(5), io.incoming.recv())
                .await
                .unwrap()
                .unwrap()
            {
                ControllerMsg::Ping { t } => io.out.send(&HostMsg::Pong { t }).unwrap(),
                ControllerMsg::Ack { seq } => {
                    assert_eq!(seq, 1);
                    got_ack = true;
                }
                ControllerMsg::VideoFeedback(f) => {
                    assert!(!got_feedback);
                    assert!(model.feedback(f));
                    assert_eq!(model.latest_feedback().unwrap().decode_time_us, 7000);
                    got_feedback = true;
                }
                ControllerMsg::View(v) => {
                    assert_eq!(v.request_id, 1);
                    assert!(model.view(v));
                    let s = model
                        .select(admitted.settings, &[Codec::H264], 1920, 1080)
                        .unwrap();
                    assert_eq!((s.request_id, s.settings.fps), (1, 15));
                    got_view = true;
                }
                m => panic!("unexpected message: {m:?}"),
            }
        }
        drop(model);
        io.close(session_id, Ok(())).await.unwrap();
    });
    let (packets, mut packet_rx) = mpsc::unbounded_channel();
    let (configs, mut config_rx) = mpsc::unbounded_channel();
    let ui = Arc::new(Ui { packets, configs });
    let svc = ControllerService::start(
        Arc::new(cfg),
        tokio::runtime::Handle::current(),
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        Identity::generate(),
    );
    let session = timeout(Duration::from_secs(5), async {
        loop {
            let result = svc
                .connect(
                    &id.code.to_string(),
                    ConnectOptions {
                        receiver: Some(receiver.clone()),
                        settings: QualitySettings {
                            codec: Codec::AV1,
                            fps: 60,
                            max_short: 0,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ui.clone(),
                    Frontend::Browser,
                )
                .await;
            match result {
                Err(Problem::NotConnected) => tokio::time::sleep(Duration::from_millis(10)).await,
                r => break r.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    let packet = timeout(Duration::from_secs(5), packet_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let config = timeout(Duration::from_secs(5), config_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(VideoHeader::context(&packet), Some(config.context));
    assert_eq!(media::payload(&packet), Some(b"encoded frame".as_slice()));
    assert_eq!(
        (config.width, config.height, config.fps, config.bitrate_kbps),
        (1280, 720, 30, 2000)
    );
    let feedback = VideoFeedback {
        context: config.context,
        sample_id: 1,
        received_seq: 1,
        decoded_seq: Some(1),
        presented_seq: Some(1),
        pending_frames: 0,
        oldest_pending_ms: 0,
        decode_time_us: 7000,
        dropped_frames: 0,
        decoder_errors: 0,
    };
    svc.command(Command::VideoFeedback {
        sid: session.sid,
        feedback,
    })
    .unwrap();
    svc.ack(session.sid, 1).unwrap();
    svc.command(Command::Settings {
        sid: session.sid,
        settings: QualitySettings {
            fps: 15,
            ..Default::default()
        },
    })
    .unwrap();
    host_task.await.unwrap();
}
