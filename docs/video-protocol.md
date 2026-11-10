# Video negotiation and receiver feedback

[Development](development.md) · [简体中文](video-protocol.zh-CN.md)

The extension uses session feature `VIDEO_MODEL` (bit 2) on signal v9/session v1. Existing message IDs, transport ACK semantics and the 24-byte media prefix remain compatible. Definitions live in `crates/airhand-proto/src/video.rs`; per-session state lives in `crates/airhand-link/src/video.rs`. Rust, TypeScript and Kotlin share JSON and media fixtures.

This stage implements capability negotiation, view preferences, selected configurations, configuration IDs on video packets and receiver feedback approximately every 200 ms. Hosts retain the latest valid feedback for two seconds. Existing ABR still uses transport ACKs and blocking; a policy that adjusts resolution/frame rate from decoder feedback remains a separate implementation and tuning task. Replaceable media transports are reserved, not enabled.

**Model responsibilities**

| Model | Origin | Meaning |
|---|---|---|
| `ReceiverCapabilities` | Controller | Accepted codecs and known decoder limits |
| `ViewRequest` | Controller | Desired output and an increasing request ID |
| `VideoConfig` | Host | Selected output parameters and their identity |
| `VideoFeedback` | Controller | Receive, decode and render-submission progress for that identity |

Capabilities contain 1–16 distinct codecs with optional maximum width, height and frame rate. Known dimensions must be at least 64 and frame rates 1–120. Null/absent limits mean unknown, not unlimited support. Independent maxima are necessary bounds; they do not guarantee that every size/rate combination works or describe measured throughput. Browsers leave unavailable limits null; Android reports ranges from its selected decoder and retains the existing size-combination checks.

View preferences reuse `QualitySettings`, but cannot override capabilities. The handshake's request ID is zero. The shared controller turns later settings commands into monotonically numbered View messages when negotiated. Hosts intersect receiver and encoder codecs and constrain dimensions/frame rate. No common codec is an explicit failure, not an implicit H.264 fallback. Actual encoder output is checked too. Capabilities are fixed for this session; changes require renegotiating a session, while preferences can change in place.

A config's frame rate is the selected ceiling and its bitrate is an encoder target, not measured output. A host can produce several configurations for one request as ABR, encoder fallback or sharing changes the result. Request ID identifies the view used for selection, not a promise that all preferences were satisfied. Android carries selection snapshots through JNI and back with encoder output so pending changes cannot relabel old output.

**Identity and framing**

`VideoContext` contains a nonzero, increasing, never-reused `configId`, plus `MediaInstance { streamId, generation }`. Changing actual parameters/the selected request, or rebuilding an encoder with identical parameters, allocates a new config ID. The existing session-wide frame sequence and cumulative ACK do not restart.

Only instance `(0, 0)` is currently accepted. Configuration identity and transport generation are separate: a bitrate change need not replace a stream, while replacing a stream must create independent cryptographic nonce space.

Negotiated video packets extend the header to 36 bytes:

| Offset | Content |
|---:|---|
| 0–23 | Existing prefix; video flags bit 1 indicates the extension, header length is 36 |
| 24 | config ID, u32 LE |
| 28 | logical stream ID, u32 LE |
| 32 | transport generation, u32 LE |
| 36 onward | Existing Annex-B/OBU payload |

Keyframe remains bit 0. Payload readers always use the advertised header length. Legacy sessions send 24-byte headers and no new control messages.

Configurations and video can arrive in either order. Packet context determines feedback association. The current decoder still uses the base header and in-band codec configuration, so it does not wait for a control-lane config notification. Frontends retain the newest config and ignore old-context decoder callbacks for telemetry.

**Feedback semantics**

Feedback is a cumulative snapshot with an increasing sample ID within its context. The command queue keeps only the latest snapshot. Counters restart with a new context; session reset cancels sampling. Changed state or pending decode work produces at most one report per 200 ms; a fully processed static picture produces no repeated empty reports.

| Field | Meaning |
|---|---|
| `receivedSeq` | Latest received frame |
| `decodedSeq` | Latest decoded frame, null until one completes |
| `presentedSeq` | Latest frame submitted to Canvas/Surface, null until submission |
| `pendingFrames` | Frames awaiting decode completion, excluding intentional A/V waiting |
| `oldestPendingMs` | Age of the oldest pending decode, including codec queuing/processing |
| `decodeTimeUs` | Smoothed submission-to-output duration, including internal codec queuing |
| `droppedFrames` | Cumulative application-discarded frames in this context |
| `decoderErrors` | Cumulative observed decoder failures in this context |

Render submission is not measured physical scanout. Timed Surface rendering can be submitted ahead of actual presentation. Durations use the receiver's monotonic clock; no subtraction of unsynchronized device clocks is required.

Hosts reject stale contexts, duplicate/regressing samples or counters, progress outside the config's issued sequence range and unreasonable durations. Feedback never releases a send window; only the existing ACK does. Missing/stale feedback alone does not disconnect a session.

A later adaptive policy should distinguish network pressure (bitrate), sustained receiver pressure (resolution/frame rate), and intentional A/V waiting. It should have persistence thresholds, recovery hysteresis and user-selected upper limits rather than rebuilding a codec on every sample.

**Registry and compatibility**

| Item | Number |
|---|---:|
| Session `VIDEO_MODEL` | bit 2 |
| Optional `ControllerIntro.video` | field 7 |
| `ControllerMsg::View` | 9 |
| `ControllerMsg::VideoFeedback` | 10 |
| `HostMsg::VideoConfig` | 12 |
| Video context flag | bit 1 |

New model structures use numbered CBOR maps. Added fields must be optional or have a defined default. Controllers advertise the feature only when their frontend supplies valid capabilities, and include an authenticated VideoOffer in the Noise intro. Claiming support without a valid offer or with a nonzero initial request ID fails the handshake.

Old hosts read legacy initial settings and skip the optional offer. New controllers use legacy settings/messages/headers unless both peers support the feature. The existing SessionInfo and Encoder frontend events remain compatible; selected configurations use a separate videoConfig event. Shared fixtures check all three frontend contracts; fixed v9 golden bytes lock both legacy and newly assigned IDs.

**Reserved media replacement contract**

Signal `MEDIA_INSTANCES` reserves bit 0 and session `MEDIA_INSTANCES` reserves bit 3. Neither is advertised in SUPPORTED. Nonzero instances are rejected; AHS2 still pairs each lane once.

A future implementation must negotiate relay support for a new stream header and route by `(sessionId, lane, streamId, generation)`, keep control alive during replacement, authenticate and acknowledge the switch, begin the new instance at an independent decode point, and bound outstanding instances/resources. Key derivation must bind direction, logical stream and generation, or provide equivalent unique nonce allocation. Resetting the counter under the same key is forbidden. End-to-end authentication must bind the instance identity and prevent replay/moving ciphertext across instances. Replacement cancellation, late old streams, reordering and old-relay fallback require tests before advertising support.

This implementation does not reopen a video lane or discard bytes already committed to its reliable stream. The reserved identity keeps configuration/feedback extensible without claiming unsupported transport behavior.
