//! 端到端加密（协议见 `airhand_proto::handshake`）：设备的身份密钥、握手、之后每一帧的加解密。
//!
//! * [`Identity`]：一台设备的长期 X25519 密钥（Noise 的静态密钥），由各端自己保存；
//! * [`initiate`] / [`Initiator::finish`]：控制端开始握手（第一条消息放进发起会话的请求里），之后在控制通道上做完
//!   （最后是控制端的确认）；[`respond`]：被控端回答（发出决定就返回，确认由会话的读任务解开）。
//!   两边都得到 [`Secured`] 和双方协商好的 [`Negotiated`]；
//! * [`Secured`]：握手一次性派生的通道密钥：每条通道每个方向一个 [`Sealer`] / [`Opener`]，声音数据报、
//!   控制数据报各一对，探测两条路的往返时延用的一对（[`Secured::probe`]）。
//!
//! 加解密都是原地做的：发送的帧预留了标签和计数的位置（`airhand_proto::framing::encode_with_spare`、
//! `airhand_proto::media::frame`），收到的帧解密之后截掉尾巴，画面包照样原样交给界面，不多拷贝。

use std::future::Future;
use std::time::Duration;

use crate::wire::{read_frame, write_frame};
use airhand_proto::framing::{self, PREFIX};
use airhand_proto::handshake::{
    self, Confirm, ControllerIntro, HandshakeError, HandshakeFinish, HandshakeInit, HandshakeReply,
    HostVerdict, MAX_MSG, NONCE_LEN, PAKE_CONTROLLER, PAKE_HOST, Pattern, PublicKey, Refusal,
    SEAL_OVERHEAD, TAG_LEN,
};
use airhand_proto::session::QualitySettings;
use airhand_proto::{Features, Role, SESSION_MIN, SESSION_VERSION, SessionLane, datagram};
use anyhow::{Context, Result, anyhow, bail};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use spake2::{Ed25519Group, Identity as PakeId, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

pub use airhand_proto::handshake::Negotiated;

use crate::Problem;

/// 握手的每一步最多等这么久（被控端的用户同意不在握手里，在之后的会话里）
pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);

// ───────────────────────────── 身份 ─────────────────────────────

/// 一台设备的长期 X25519 密钥对（Noise 的静态密钥）。只保存私钥（32 字节），公钥由它算出
#[derive(Clone)]
pub struct Identity {
    private: [u8; 32],
    public: PublicKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("public", &hex(&self.public))
            .finish()
    }
}

impl Identity {
    pub fn generate() -> Self {
        Self::from_private(rand::random())
    }

    pub fn from_private(private: [u8; 32]) -> Self {
        let public = curve25519_dalek::MontgomeryPoint::mul_base_clamped(private).to_bytes();
        Self { private, public }
    }

    /// 保存用的私钥（调用方负责加密保存，和取回控制码的密钥一样）
    pub fn private(&self) -> &[u8; 32] {
        &self.private
    }

    pub fn public(&self) -> &PublicKey {
        &self.public
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.private = [0; 32];
    }
}

/// 公钥写成 64 位小写十六进制（保存、显示、传给界面）
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// 解析 [`hex`] 写出的公钥 / 私钥
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

// ───────────────────────────── 每一帧 ─────────────────────────────

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::assume_unique_for_key(n)
}

fn aead_key(raw: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, raw).expect("32-byte key"))
}

/// 一条通道一个方向的发送端。每个密封单元是 `密文 | 标签 | 计数`，计数就是 nonce，从 0 开始递增
/// （见 `airhand_proto::handshake` 的"之后的每一帧"）
pub struct Sealer {
    key: LessSafeKey,
    counter: u64,
}

impl Sealer {
    /// 把一帧（`u32 长度 | 明文`，`framing::encode_with_spare` / `media::frame` 编好的，留了 [`SEAL_OVERHEAD`]
    /// 的容量）原地加密成 `u32 长度 | 密文 | 标签 | 计数`
    pub fn seal_frame(&mut self, frame: &mut Vec<u8>) -> Result<()> {
        anyhow::ensure!(frame.len() >= PREFIX, "not a frame");
        self.seal_at(frame, PREFIX)?;
        let len = (frame.len() - PREFIX) as u32;
        frame[..PREFIX].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }

    /// 加密一段负载（不带长度前缀），返回密封单元 `密文 | 标签 | 计数`
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(plaintext.len() + SEAL_OVERHEAD);
        out.extend_from_slice(plaintext);
        self.seal_at(&mut out, 0)?;
        Ok(out)
    }

    /// 数据报：`会话 ID | 种类 | 密文 | 标签 | 计数`（见 `airhand_proto::datagram`）
    pub fn seal_datagram(&mut self, session_id: u64, kind: u8, packet: &[u8]) -> Result<Vec<u8>> {
        let mut d = Vec::with_capacity(datagram::HEADER + packet.len() + SEAL_OVERHEAD);
        d.extend_from_slice(&session_id.to_le_bytes());
        d.push(kind);
        d.extend_from_slice(packet);
        self.seal_at(&mut d, datagram::HEADER)?;
        Ok(d)
    }

    /// 把 `buf[start..]` 原地加密，后面追加标签和计数
    fn seal_at(&mut self, buf: &mut Vec<u8>, start: usize) -> Result<()> {
        let c = self.counter;
        self.counter = c.checked_add(1).context("nonce counter exhausted")?;
        let tag = self
            .key
            .seal_in_place_separate_tag(nonce(c), Aad::empty(), &mut buf[start..])
            .map_err(|_| anyhow!("encryption failed"))?;
        buf.extend_from_slice(tag.as_ref());
        buf.extend_from_slice(&c.to_le_bytes());
        Ok(())
    }
}

/// 密封单元（`密文 | 标签 | 计数`）末尾的计数，不用解密就看得到：几条路上来的帧按它排好再解密
/// （见 `crate::merge`）。短得不像一个密封单元时为 `None`
pub fn counter_of(sealed: &[u8]) -> Option<u64> {
    let at = sealed.len().checked_sub(NONCE_LEN)?;
    (sealed.len() >= SEAL_OVERHEAD).then(|| u64::from_le_bytes(sealed[at..].try_into().unwrap()))
}

/// 一条通道一个方向的接收端
pub struct Opener {
    key: LessSafeKey,
    order: Order,
}

/// 接收端认哪些计数（见 `airhand_proto::handshake` 的"之后的每一帧"）
enum Order {
    /// 控制、大块通道：必须是下一个，丢掉、重排、重放的消息都不认
    Next(u64),
    /// 画面、声音、探测：没收到过就行（滑动窗口挡掉重放）
    Window(ReplayWindow),
}

impl Opener {
    /// 下一个要的计数（控制、大块通道）；画面、声音是收到过的最大计数 + 1
    pub(crate) fn expected(&self) -> u64 {
        match &self.order {
            Order::Next(n) => *n,
            Order::Window(w) => w.top,
        }
    }

    /// 这个计数还没收到过（画面、声音的滑动窗口；控制、大块通道必须正好是下一个）
    pub(crate) fn is_fresh(&self, c: u64) -> bool {
        match &self.order {
            Order::Next(n) => c == *n,
            Order::Window(w) => w.fresh(c),
        }
    }

    /// 原地解开一个密封单元（`密文 | 标签 | 计数`，流上一帧去掉长度前缀的部分），之后 `buf` 就是明文。
    /// 解不开（被改过、不是这条通道的、计数不对）是错误：控制、大块、画面通道上会话不能再继续；
    /// 数据报丢掉就是了
    pub fn open(&mut self, buf: &mut Vec<u8>) -> Result<()> {
        let at = buf
            .len()
            .checked_sub(NONCE_LEN + TAG_LEN)
            .context("a session frame is too short")?
            + TAG_LEN;
        let c = u64::from_le_bytes(buf[at..].try_into().expect("8 bytes"));
        anyhow::ensure!(
            self.is_fresh(c),
            "a session frame was dropped, reordered or replayed"
        );
        buf.truncate(at);
        let len = self
            .key
            .open_in_place(nonce(c), Aad::empty(), buf)
            .map_err(|_| anyhow!("a session frame failed authentication"))?
            .len();
        buf.truncate(len);
        // 认证通过之后才记下：伪造的单元不能把计数推走
        match &mut self.order {
            Order::Next(next) => *next = c.saturating_add(1),
            Order::Window(w) => w.accept(c),
        }
        Ok(())
    }
}

/// 最近 128 个计数里收过哪些（RFC 4303 的做法）
#[derive(Default)]
struct ReplayWindow {
    /// 收到过的最大计数 + 1（0 表示还没收到过）
    top: u64,
    /// 第 i 位：top - 1 - i 收到过
    seen: u128,
}

impl ReplayWindow {
    fn fresh(&self, c: u64) -> bool {
        if c >= self.top {
            return true;
        }
        let back = self.top - 1 - c;
        back < 128 && self.seen & (1 << back) == 0
    }

    fn accept(&mut self, c: u64) {
        if c >= self.top {
            let shift = c - self.top + 1;
            self.seen = if shift >= 128 { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.top = c.saturating_add(1);
        } else {
            self.seen |= 1 << (self.top - 1 - c);
        }
    }
}

// ───────────────────────────── 会话的密钥 ─────────────────────────────

/// 一条通道（或者一种数据报）一个方向一对加密端。没有 Clone，也没有重新从计数 0 创建加密端的公开接口：
/// 每对密钥只能移交一次
pub struct LaneKeys {
    pub sealer: Sealer,
    pub opener: Opener,
}

/// 握手之后移交给各条通道的密钥；控制通道的在 [`Initiated`] / [`Responded`] 里（握手已经用掉了它的第一个计数）。
pub struct Secured {
    /// 对方的公钥
    pub peer: PublicKey,
    /// 安全码（两端一样，见 `airhand_proto::handshake::safety_code`）
    pub safety: String,
    pub video: LaneKeys,
    pub bulk: LaneKeys,
    /// 声音数据报。乱序可以、重放不行
    pub audio: LaneKeys,
    /// 控制数据报（Ping、Pong、画面确认，`airhand_proto::datagram::CONTROL`）。乱序可以、重放不行：
    /// 直连通着时同一个数据报两条路各发一份，后到的那份就是重放
    pub datagrams: LaneKeys,
    /// 探测数据报（`airhand_proto::datagram::PROBE`）：量中转、直连两条路的往返时延。乱序可以、重放不行
    pub probe: LaneKeys,
}

/// 仅握手内部持有的原始材料，消费后派生一次全部通道密钥。
struct KeyMaterial {
    /// 本端是哪一边（控制端是 Noise 的发起方）
    role: Role,
    /// 发起方 → 响应方、响应方 → 发起方
    i2r: [u8; 32],
    r2i: [u8; 32],
    /// 握手的哈希（派生用的 salt）
    h: [u8; 32],
    peer: PublicKey,
    safety: String,
}

impl Drop for KeyMaterial {
    fn drop(&mut self) {
        self.i2r = [0; 32];
        self.r2i = [0; 32];
    }
}

/// 每条通道、每种数据报的密钥用不同的标签派生：从别的通道搬过来的帧解不开
fn label(lane: SessionLane) -> &'static [u8] {
    match lane {
        SessionLane::Control => b"airhand control",
        SessionLane::Video => b"airhand video",
        SessionLane::Bulk => b"airhand bulk",
    }
}

impl KeyMaterial {
    fn new(role: Role, hs: &mut snow::HandshakeState) -> Result<Self> {
        let peer: PublicKey = hs
            .get_remote_static()
            .context("the peer has no static key")?
            .try_into()
            .context("bad static key")?;
        let h: [u8; 32] = hs
            .get_handshake_hash()
            .try_into()
            .context("unexpected handshake hash length")?;
        let (i2r, r2i) = hs.dangerously_get_raw_split();
        let mut seed = [0u8; 32];
        derive(&h, &h, b"airhand safety code", &mut seed);
        Ok(Self {
            role,
            i2r,
            r2i,
            h,
            peer,
            safety: handshake::safety_code(&seed),
        })
    }

    /// 全部通道的密钥，和控制通道的
    fn split(self) -> (Secured, LaneKeys) {
        let window = || Order::Window(ReplayWindow::default());
        (
            Secured {
                peer: self.peer,
                safety: self.safety.clone(),
                video: self.keys(label(SessionLane::Video), window()),
                bulk: self.keys(label(SessionLane::Bulk), Order::Next(0)),
                audio: self.keys(b"airhand audio", window()),
                datagrams: self.keys(b"airhand control datagram", window()),
                probe: self.keys(b"airhand probe", window()),
            },
            self.keys(label(SessionLane::Control), Order::Next(0)),
        )
    }

    fn key(&self, sending: bool, label: &[u8]) -> LessSafeKey {
        // 控制端发送 = 发起方 → 响应方
        let i2r = sending == (self.role == Role::Controller);
        let ikm = if i2r { &self.i2r } else { &self.r2i };
        let mut out = [0u8; 32];
        derive(&self.h, ikm, label, &mut out);
        aead_key(&out)
    }

    fn keys(&self, label: &[u8], order: Order) -> LaneKeys {
        LaneKeys {
            sealer: Sealer {
                key: self.key(true, label),
                counter: 0,
            },
            opener: Opener {
                key: self.key(false, label),
                order,
            },
        }
    }
}

/// 测试用：一条通道一个方向的一对加密端（发送端、接收端），不用握手。`ordered`：控制、大块通道那样必须一个接一个
#[cfg(test)]
pub(crate) fn test_lane(ordered: bool) -> (Sealer, Opener) {
    let raw = [7u8; 32];
    let order = if ordered {
        Order::Next(0)
    } else {
        Order::Window(ReplayWindow::default())
    };
    (
        Sealer {
            key: aead_key(&raw),
            counter: 0,
        },
        Opener {
            key: aead_key(&raw),
            order,
        },
    )
}

/// HKDF-SHA256：`salt` 提取、`info` 扩展出 32 字节
fn derive(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8; 32]) {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    prk.expand(&[info], hkdf::HKDF_SHA256)
        .and_then(|okm| okm.fill(out))
        .expect("32 bytes is a valid HKDF-SHA256 output length");
}

// ───────────────────────────── 握手 ─────────────────────────────

/// 认识的模式的 Noise 参数（不认识的模式在这之前就回答了 [`HandshakeError::Unsupported`]）
fn noise(pattern: Pattern) -> Result<snow::params::NoiseParams> {
    let name = pattern
        .noise()
        .with_context(|| format!("unsupported handshake pattern {pattern:?}"))?;
    Ok(name.parse().expect("valid noise pattern"))
}

async fn read_step<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Result<()> {
    let got = tokio::time::timeout(STEP_TIMEOUT, read_frame(r, MAX_MSG, buf))
        .await
        .context("the end-to-end handshake timed out")??;
    anyhow::ensure!(got, "the session closed during the end-to-end handshake");
    Ok(())
}

async fn send_step<W: AsyncWrite + Unpin, T: airhand_proto::minicbor::Encode<()>>(
    w: &mut W,
    msg: &T,
) -> Result<()> {
    tokio::time::timeout(STEP_TIMEOUT, write_frame(w, &framing::to_vec(msg)))
        .await
        .context("the end-to-end handshake writer timed out")??;
    Ok(())
}

/// Noise 消息的缓冲区（Noise 规定一条消息最多 65535 字节）
fn noise_buf() -> Vec<u8> {
    vec![0u8; 65535]
}

/// 控制端握手的结果
pub struct Initiated {
    pub secured: Secured,
    /// 控制通道上被控端 → 控制端的接收端（已经解开了被控端的决定，计数接着往下数）
    pub opener: Opener,
    /// 控制通道上控制端 → 被控端的发送端（已经发出了确认，计数接着往下数）
    pub sealer: Sealer,
    /// 选定的会话版本、双方都支持的能力位（和被控端算出来的一样）
    pub negotiated: Negotiated,
}

/// 控制端怎么证明自己、怎么认被控端
pub struct InitiatorParams<'a> {
    /// 控制码（配对时算进 SPAKE2 的密码）
    pub code: u32,
    pub me: &'a Identity,
    /// 之前记下的被控端公钥（"我的设备"）：只认它
    pub host_key: Option<PublicKey>,
    /// 配对码（被控端要求配对、或者对方的身份变了时用户输入的）。带着它就用配对码认对方，
    /// 对方的公钥可以和 `host_key` 不一样（重新配对之后记下新的，见 `airhand_proto::handshake` 的"身份"）
    pub pairing: Option<&'a str>,
    pub intro: &'a ControllerIntro,
}

/// 控制端发出了第一条消息、在等被控端回答的握手（见 [`initiate`]）
pub struct Initiator {
    hs: snow::HandshakeState,
    pattern: Pattern,
    pake: Option<Spake2<Ed25519Group>>,
    /// 介绍（CBOR），`Noise_XX` 时放在第 3 条里
    intro: Vec<u8>,
    features: Features,
}

/// 控制端：开始握手，返回第一条消息（[`HandshakeInit`] 的 CBOR，放进 `ClientMsg::Connect::init`）。
/// 接着在控制通道上用 [`Initiator::finish`] 做完
pub fn initiate(p: InitiatorParams<'_>) -> Result<(Initiator, Vec<u8>)> {
    let pattern = match (p.pairing, p.host_key) {
        (Some(_), _) => Pattern::Pairing,
        (None, Some(_)) => Pattern::Known,
        (None, None) => Pattern::First,
    };
    let pake = p.pairing.map(|code| {
        Spake2::<Ed25519Group>::start_a(
            &Password::new(handshake::pake_password(p.code, code)),
            &PakeId::new(PAKE_CONTROLLER),
            &PakeId::new(PAKE_HOST),
        )
    });
    let mut init = HandshakeInit {
        pattern,
        pake: pake.as_ref().map(|(_, msg)| msg.clone()),
        noise: Vec::new(),
    };
    let prologue = handshake::prologue(&init);
    let mut builder = snow::Builder::new(noise(pattern)?)
        .local_private_key(p.me.private())?
        .prologue(&prologue)?;
    if let (Pattern::Known, Some(k)) = (pattern, &p.host_key) {
        builder = builder.remote_public_key(k)?;
    }
    let mut hs = builder.build_initiator()?;
    let intro = framing::to_vec(p.intro);
    let mut msg = noise_buf();
    // IK：介绍放在第 1 条里（加密、认证过）；XX：第 1 条什么都不能带
    let first: &[u8] = if pattern == Pattern::Known {
        &intro
    } else {
        &[]
    };
    let n = hs.write_message(first, &mut msg)?;
    init.noise = msg[..n].to_vec();
    let init = framing::to_vec(&init);
    anyhow::ensure!(init.len() <= MAX_MSG, "the handshake message is too large");
    Ok((
        Initiator {
            hs,
            pattern,
            pake: pake.map(|(s, _)| s),
            intro,
            features: p.intro.features,
        },
        init,
    ))
}

impl Initiator {
    /// 读被控端的回答、做完握手（见 `airhand_proto::handshake`）。`recv` / `send`：控制通道收、发的两半。
    /// 失败时的错误是 [`Problem`]（对方身份变了、要配对码、配对码不对、版本不兼容……）或者普通的网络错误
    pub async fn finish<R, W>(self, recv: &mut R, send: &mut W) -> Result<Initiated>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let Self {
            mut hs,
            pattern,
            pake,
            intro,
            features,
        } = self;
        let mut buf = Vec::new();
        read_step(recv, &mut buf).await?;
        let reply: HandshakeReply = framing::decode_known(&buf)?;
        if let Some(e) = reply.error {
            return Err(handshake_problem(e, pattern, reply.patterns.as_deref()).into());
        }
        if let Some(spake) = pake {
            let theirs = reply
                .pake
                .as_deref()
                .context("the host didn't answer the pairing")?;
            let psk = spake
                .finish(theirs)
                .map_err(|_| anyhow!("bad pairing message from the host"))?;
            hs.set_psk(3, &psk)?;
        }
        let msg2 = reply
            .noise
            .as_deref()
            .context("the host sent no handshake message")?;
        let mut payload = noise_buf();
        hs.read_message(msg2, &mut payload)
            .map_err(|_| -> anyhow::Error {
                // IK：被控端用的不是我们记着的密钥（换过身份，或者是别人冒充的）
                if pattern == Pattern::Known {
                    Problem::HostKeyChanged.into()
                } else {
                    anyhow!("the host's handshake message failed authentication")
                }
            })?;

        let verdict = if pattern == Pattern::Known {
            reply.verdict
        } else {
            // First：之前不认识它；Pairing：配对码认的是它，换过身份也没关系
            let mut msg = noise_buf();
            let n = hs.write_message(&intro, &mut msg)?;
            send_step(
                send,
                &HandshakeFinish {
                    noise: msg[..n].to_vec(),
                },
            )
            .await?;
            read_step(recv, &mut buf).await?;
            let reply: HandshakeReply = framing::decode_known(&buf)?;
            if let Some(e) = reply.error {
                return Err(handshake_problem(e, pattern, reply.patterns.as_deref()).into());
            }
            reply.verdict
        };
        anyhow::ensure!(hs.is_handshake_finished(), "the handshake didn't finish");
        let (
            secured,
            LaneKeys {
                mut opener,
                mut sealer,
            },
        ) = KeyMaterial::new(Role::Controller, &mut hs)?.split();
        let mut verdict = verdict.context("the host sent no verdict")?;
        opener.open(&mut verdict)?;
        let verdict: HostVerdict = framing::decode_known(&verdict)?;
        let version = match (verdict.version, verdict.refused) {
            (Some(v), _) => v,
            (None, Some(Refusal::PairingRequired)) => return Err(Problem::PairingRequired.into()),
            (None, Some(Refusal::Incompatible)) => {
                return Err(incompatible(verdict.min, verdict.max).into());
            }
            (None, r) => {
                return Err(
                    Problem::failed(format!("the host refused the connection ({r:?})")).into(),
                );
            }
        };
        anyhow::ensure!(
            (SESSION_MIN..=SESSION_VERSION).contains(&version),
            "the host chose session version {version}, which we can't speak"
        );
        let mut negotiated = Negotiated::new(version, verdict.features);
        negotiated.features = negotiated.features.common(features);
        // 确认（见 airhand_proto::handshake 的"确认"）：被控端解开它才算本机进来了，重放的握手消息走不到这一步
        let mut confirm = framing::encode_with_spare(&Confirm::default(), SEAL_OVERHEAD);
        sealer.seal_frame(&mut confirm)?;
        tokio::time::timeout(STEP_TIMEOUT, send.write_all(&confirm))
            .await
            .context("the handshake confirmation writer timed out")??;
        debug!(?pattern, ?negotiated, safety = %secured.safety, "end-to-end handshake done");
        Ok(Initiated {
            negotiated,
            secured,
            opener,
            sealer,
        })
    }
}

/// 会话版本不兼容：`min..=max` 是对方能说的
fn incompatible(min: u16, max: u16) -> Problem {
    if max < SESSION_MIN {
        // 对方太旧
        Problem::PeerIncompatible {
            peer: max,
            ours: SESSION_MIN,
        }
    } else {
        Problem::PeerIncompatible {
            peer: min,
            ours: SESSION_VERSION,
        }
    }
}

/// 被控端回答的握手错误。`theirs`：被控端会的模式（[`HandshakeError::Unsupported`] 时有）
fn handshake_problem(e: HandshakeError, pattern: Pattern, theirs: Option<&[Pattern]>) -> Problem {
    match e {
        HandshakeError::Unsupported => {
            // 模式的编号越大越新。对方会的都比这次用的旧：对方该更新了；
            // 否则（对方会更新的模式、或者不再支持这个旧的）是本机该更新了
            let ours = pattern.code();
            let newest = Pattern::newest(theirs.unwrap_or_default());
            let narrow = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            if ours > newest {
                Problem::PeerIncompatible {
                    peer: narrow(newest),
                    ours: narrow(ours),
                }
            } else {
                Problem::PeerIncompatible {
                    peer: narrow(newest.max(ours + 1)),
                    ours: narrow(ours),
                }
            }
        }
        HandshakeError::UnknownKey if pattern == Pattern::Known => Problem::HostKeyChanged,
        HandshakeError::PairingFailed => Problem::PairingFailed,
        HandshakeError::PairingUnavailable => Problem::PairingUnavailable,
        e => Problem::failed(format!("end-to-end handshake failed: {e:?}")),
    }
}

/// 被控端怎么对待没配对过的控制端
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 知道控制码就能连（等于信任中转服务器：服务器知道所有控制码）
    Open,
    /// 请本机用户同意（同意之后在这次运行里记住它）
    Approval,
    /// 要配对码（配对之后记住它）
    Pairing,
}

/// 一个已占名额的配对尝试。实现必须把失败和在途尝试一起计入本机预算；
/// 丢弃（包括超时、取消、格式错误）算一次未成功尝试。换配对码后旧尝试不能提交。
pub trait PairingAttempt: Send {
    fn code(&self) -> &str;
    /// 在本机同一把锁下检查代际并记住控制端，成功才返回 true。
    fn complete(self: Box<Self>, key: PublicKey, name: &str) -> bool;
}

/// 被控端：自己是谁，认得哪些控制端，配对尝试的本机预算（各平台自己保存）。
pub trait Gate: Send + Sync + 'static {
    fn identity(&self) -> Identity;
    fn access(&self) -> Access;
    /// 原子地占一个配对名额；None 表示不接受配对或预算已用完。
    fn begin_pairing(&self) -> Option<Box<dyn PairingAttempt + '_>> {
        None
    }
    /// 这个控制端配对过（或者本机用户这次运行里同意过）
    fn is_known(&self, key: &PublicKey) -> bool;
}

/// 被控端为什么让这个控制端进来
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// 刚刚用配对码配对成功
    Paired,
    /// 配对过的 / 本机用户同意过的
    Known,
    /// 被控端允许任何知道控制码的人连
    Open,
    /// 还要请本机用户同意（会话里先发 `AwaitingApproval`）
    NeedsApproval,
}

/// 被控端握手的结果。控制端还没确认（见 `airhand_proto::handshake` 的"确认"）：`opener` 解开的第一帧必须是
/// [`Confirm`]，在那之前不能显示"谁在连"、不能请本机用户同意
pub struct Responded {
    pub secured: Secured,
    /// 控制通道上被控端 → 控制端的发送端（已经发出了决定，计数接着往下数）
    pub sealer: Sealer,
    /// 控制通道上控制端 → 被控端的接收端（下一帧是确认，计数 0）
    pub opener: Opener,
    pub intro: ControllerIntro,
    pub admission: Admission,
    /// 选定的会话版本、双方都支持的能力位（和控制端算出来的一样）
    pub negotiated: Negotiated,
}

pub struct ResponderParams<'a> {
    /// 本机的控制码（配对时算进 SPAKE2 的密码）
    pub code: u32,
    pub gate: &'a dyn Gate,
}

/// 被控端：回答控制端的握手（`init` 是 `ServerMsg::Incoming::init`，见 `airhand_proto::handshake`）。
/// 回答写进 `send`（本端的控制通道）；控制端之后的消息从 `recv` 来（控制端的控制通道，要等它开过来：
/// 用到时才等）。让不让进来由 [`Gate`] 决定；拒绝时已经把原因告诉了控制端，返回错误。
///
/// 发出决定就返回，不等控制端的确认（见 [`Responded`]）。用到了 `recv` 的话（`Noise_XX` 要读第 3 条）连同它等到的流
/// 一起返回，`Noise_IK` 用不着它，是 `None`
pub async fn respond<W, R, F>(
    init: &[u8],
    send: &mut W,
    recv: F,
    p: ResponderParams<'_>,
) -> Result<(Responded, Option<R>)>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    F: Future<Output = Result<R>>,
{
    let init: HandshakeInit = match framing::decode_known(init) {
        Ok(i) if init.len() <= MAX_MSG => i,
        Ok(_) => {
            fail(send, HandshakeError::Malformed).await;
            bail!("the controller's first handshake message is too large");
        }
        Err(e) => {
            fail(send, HandshakeError::Malformed).await;
            return Err(e.into());
        }
    };
    let pattern = init.pattern;
    if !pattern.is_known() {
        // 以后的模式：告诉控制端本机会哪些，它据此判断该更新哪一边
        let _ = send_step(
            send,
            &HandshakeReply {
                error: Some(HandshakeError::Unsupported),
                patterns: Some(Pattern::KNOWN.to_vec()),
                ..Default::default()
            },
        )
        .await;
        bail!("unsupported end-to-end handshake pattern {pattern:?}");
    }
    let me = p.gate.identity();
    let mut reply = HandshakeReply::default();
    let mut psk = None;
    let mut pairing = None;
    if pattern == Pattern::Pairing {
        let Some(attempt) = p.gate.begin_pairing() else {
            fail(send, HandshakeError::PairingUnavailable).await;
            bail!("a controller tried to pair, but pairing is off");
        };
        let password = handshake::pake_password(p.code, attempt.code());
        pairing = Some(attempt);
        let (spake, msg_b) = Spake2::<Ed25519Group>::start_b(
            &Password::new(password),
            &PakeId::new(PAKE_CONTROLLER),
            &PakeId::new(PAKE_HOST),
        );
        let theirs = init.pake.as_deref().unwrap_or_default();
        match spake.finish(theirs) {
            Ok(k) => psk = Some(k),
            Err(_) => {
                fail(send, HandshakeError::Malformed).await;
                bail!("bad pairing message");
            }
        }
        reply.pake = Some(msg_b);
    }
    let prologue = handshake::prologue(&init);
    let mut hs = snow::Builder::new(noise(pattern)?)
        .local_private_key(me.private())?
        .prologue(&prologue)?
        .build_responder()?;
    if let Some(psk) = &psk {
        hs.set_psk(3, psk)?;
    }
    let mut payload = noise_buf();
    let n = match hs.read_message(&init.noise, &mut payload) {
        Ok(n) => n,
        Err(_) => {
            // IK：控制端记着的不是我们的公钥（我们换过身份，或者它连的根本不是我们）
            let e = if pattern == Pattern::Known {
                HandshakeError::UnknownKey
            } else {
                HandshakeError::Malformed
            };
            fail(send, e).await;
            bail!("the controller's first handshake message failed authentication ({e:?})");
        }
    };
    let mut msg = noise_buf();
    let m = hs.write_message(&[], &mut msg)?;
    reply.noise = Some(msg[..m].to_vec());

    let (intro_bytes, stream) = if pattern == Pattern::Known {
        (payload[..n].to_vec(), None)
    } else {
        send_step(send, &reply).await?;
        reply = HandshakeReply::default();
        let mut r = recv.await?;
        let mut buf = Vec::new();
        read_step(&mut r, &mut buf).await?;
        let finish: HandshakeFinish = framing::decode_known(&buf)?;
        match hs.read_message(&finish.noise, &mut payload) {
            Ok(n) => (payload[..n].to_vec(), Some(r)),
            Err(_) if pattern == Pattern::Pairing => {
                drop(pairing.take());
                fail(send, HandshakeError::PairingFailed).await;
                bail!("a controller entered a wrong pairing code");
            }
            Err(_) => {
                fail(send, HandshakeError::Malformed).await;
                bail!("the controller's last handshake message failed authentication");
            }
        }
    };
    anyhow::ensure!(hs.is_handshake_finished(), "the handshake didn't finish");
    let intro: ControllerIntro = match framing::decode_known(&intro_bytes) {
        Ok(i) => i,
        Err(e) => {
            fail(send, HandshakeError::Malformed).await;
            return Err(e.into());
        }
    };
    let (secured, LaneKeys { opener, mut sealer }) = KeyMaterial::new(Role::Host, &mut hs)?.split();

    // 决定：版本，然后是让不让进来
    let version = intro.max.min(SESSION_VERSION);
    let compatible = version >= intro.min.max(SESSION_MIN);
    let negotiated = Negotiated::new(version, intro.features);
    let admission = if pattern == Pattern::Pairing {
        Some(Admission::Paired)
    } else if p.gate.is_known(&secured.peer) {
        Some(Admission::Known)
    } else {
        match p.gate.access() {
            Access::Open => Some(Admission::Open),
            Access::Approval => Some(Admission::NeedsApproval),
            Access::Pairing => None,
        }
    };
    let refused = if !compatible {
        Some(Refusal::Incompatible)
    } else if admission.is_none() {
        Some(Refusal::PairingRequired)
    } else {
        None
    };
    // 配对：解开了第 3 条，配对码就是对的、这次握手也是新鲜的（第 3 条离不开本端这次的临时密钥）：现在就记住它。
    // 配对码可能在这期间换掉了：本机在自己的锁里检查，换掉了就不认（见 airhand_proto::handshake 的"配对"）
    if let (None, Some(attempt)) = (refused, pairing) {
        if !attempt.complete(secured.peer, &intro.name) {
            fail(send, HandshakeError::PairingUnavailable).await;
            bail!("pairing was revoked during the handshake");
        }
        info!(name = %intro.name, key = %hex(&secured.peer), "a controller paired with this host");
    }
    let verdict = HostVerdict {
        version: refused.is_none().then_some(version),
        features: airhand_proto::features::session::SUPPORTED,
        refused,
        min: SESSION_MIN,
        max: SESSION_VERSION,
    };
    reply.verdict = Some(sealer.seal(&framing::to_vec(&verdict))?);
    send_step(send, &reply).await?;
    if let Some(r) = refused {
        bail!("refused the controller: {r:?}");
    }
    Ok((
        Responded {
            negotiated,
            secured,
            sealer,
            opener,
            intro,
            admission: admission.expect("checked above"),
        },
        stream,
    ))
}

/// 告诉控制端握手失败了（明文，见 `HandshakeReply::error`）。发不出去也没关系
async fn fail<W: AsyncWrite + Unpin>(send: &mut W, e: HandshakeError) {
    let _ = send_step(
        send,
        &HandshakeReply {
            error: Some(e),
            ..Default::default()
        },
    )
    .await;
}

/// 本端的介绍（控制端）：名称、系统，开始时的画质和要不要声音
pub fn controller_intro(
    name: &str,
    os: &str,
    settings: QualitySettings,
    audio: bool,
) -> ControllerIntro {
    ControllerIntro {
        min: SESSION_MIN,
        max: SESSION_VERSION,
        features: airhand_proto::features::session::SUPPORTED,
        name: name.to_string(),
        os: os.to_string(),
        settings: settings.sanitized(),
        audio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TestGate {
        me: Identity,
        access: Access,
        code: Mutex<Option<String>>,
        failures: AtomicU32,
        known: Mutex<HashSet<PublicKey>>,
    }

    impl TestGate {
        fn new(access: Access, code: Option<&str>) -> Self {
            Self {
                me: Identity::generate(),
                access,
                code: Mutex::new(code.map(Into::into)),
                failures: AtomicU32::new(0),
                known: Mutex::default(),
            }
        }
    }

    impl Gate for TestGate {
        fn identity(&self) -> Identity {
            self.me.clone()
        }
        fn access(&self) -> Access {
            self.access
        }
        fn begin_pairing(&self) -> Option<Box<dyn PairingAttempt + '_>> {
            Some(Box::new(TestAttempt {
                gate: self,
                code: self.code.lock().unwrap().clone()?,
                completed: false,
            }))
        }
        fn is_known(&self, key: &PublicKey) -> bool {
            self.known.lock().unwrap().contains(key)
        }
    }

    struct TestAttempt<'a> {
        gate: &'a TestGate,
        code: String,
        completed: bool,
    }
    impl PairingAttempt for TestAttempt<'_> {
        fn code(&self) -> &str {
            &self.code
        }
        fn complete(mut self: Box<Self>, key: PublicKey, _: &str) -> bool {
            self.gate.known.lock().unwrap().insert(key);
            self.completed = true;
            true
        }
    }
    impl Drop for TestAttempt<'_> {
        fn drop(&mut self) {
            if !self.completed {
                self.gate.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    type Pipe = (
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    );

    fn pipes() -> (Pipe, Pipe) {
        let (a, b) = tokio::io::duplex(1 << 20);
        (tokio::io::split(a), tokio::io::split(b))
    }

    fn params<'a>(
        me: &'a Identity,
        host_key: Option<PublicKey>,
        pairing: Option<&'a str>,
        intro: &'a ControllerIntro,
    ) -> InitiatorParams<'a> {
        InitiatorParams {
            code: 123_456,
            me,
            host_key,
            pairing,
            intro,
        }
    }

    fn responder(gate: &TestGate) -> ResponderParams<'_> {
        ResponderParams {
            code: 123_456,
            gate,
        }
    }

    /// 握手一次（第一条消息直接交给被控端，之后的走一对管道），被控端再解开控制端的确认。
    /// 返回控制端和被控端各自的结果
    async fn run(
        gate: &TestGate,
        me: &Identity,
        host_key: Option<PublicKey>,
        pairing: Option<&str>,
    ) -> (Result<Initiated>, Result<Responded>) {
        let ((mut cr, mut cw), (mut hr, mut hw)) = pipes();
        let intro = test_intro();
        let (initiator, init) = initiate(params(me, host_key, pairing, &intro)).unwrap();
        // 各自拿着自己那一端：一边结束（失败）时连接随之关闭，另一边马上知道
        let c = async move { initiator.finish(&mut cr, &mut cw).await };
        let h = async move {
            let (mut r, _) =
                respond(&init, &mut hw, async { Ok(&mut hr) }, responder(gate)).await?;
            // 确认是控制端在控制通道上发的第一帧密文
            let mut buf = Vec::new();
            read_frame(&mut hr, 1 << 16, &mut buf).await?;
            r.opener.open(&mut buf)?;
            let _: Confirm = framing::decode_known(&buf)?;
            Ok(r)
        };
        tokio::join!(c, h)
    }

    fn problem(r: Result<Initiated>) -> Problem {
        Problem::from_error(r.err().expect("expected a failure"), Problem::failed)
    }

    fn test_intro() -> ControllerIntro {
        let settings = QualitySettings {
            fps: 60,
            ..QualitySettings::default()
        };
        controller_intro("ctl", "test", settings, true)
    }

    #[tokio::test]
    async fn first_contact_then_known_key() {
        let gate = TestGate::new(Access::Open, None);
        let me = Identity::generate();
        let (c, h) = run(&gate, &me, None, None).await;
        let (c, h) = (c.unwrap(), h.unwrap());
        assert_eq!(c.secured.peer, *gate.me.public());
        assert_eq!(h.secured.peer, *me.public());
        assert_eq!(c.secured.safety, h.secured.safety);
        assert_eq!(h.admission, Admission::Open);
        assert_eq!(h.intro.name, "ctl");
        // 开始时的画质和声音在介绍里
        assert_eq!((h.intro.settings.fps, h.intro.audio), (60, true));
        // 两边协商出来的一样
        assert_eq!(c.negotiated, h.negotiated);
        assert_eq!(c.negotiated.version, SESSION_VERSION);

        // 两边的密钥对得上
        let mut c_send = c.secured.bulk.sealer;
        let mut h_recv = h.secured.bulk.opener;
        let mut frame = framing::encode_with_spare(
            &airhand_proto::session::ControllerMsg::Clipboard { text: "hi".into() },
            SEAL_OVERHEAD,
        );
        let cap = frame.capacity();
        c_send.seal_frame(&mut frame).unwrap();
        assert_eq!(frame.capacity(), cap, "sealing reallocated");
        let mut payload = frame[4..].to_vec();
        h_recv.open(&mut payload).unwrap();
        let mut h_send = h.secured.video.sealer;
        let mut c_recv = c.secured.video.opener;
        let mut frame = airhand_proto::media::frame(&[7; 24], &[b"hello"]);
        h_send.seal_frame(&mut frame).unwrap();
        let mut payload = frame[4..].to_vec();
        c_recv.open(&mut payload).unwrap();
        assert_eq!(&payload[24..], b"hello");
        // 控制通道：被控端的发送端接着决定往下数，控制端的发送端接着确认往下数
        let (mut h_send, mut c_recv) = (h.sealer, c.opener);
        let mut f = framing::encode(&airhand_proto::session::HostMsg::AwaitingApproval);
        h_send.seal_frame(&mut f).unwrap();
        let mut p = f[4..].to_vec();
        c_recv.open(&mut p).unwrap();
        let (mut c_send, mut h_recv) = (c.sealer, h.opener);
        let mut f =
            framing::encode(&airhand_proto::session::ControllerMsg::RequestKeyframe { after: 1 });
        c_send.seal_frame(&mut f).unwrap();
        let mut p = f[4..].to_vec();
        h_recv.open(&mut p).unwrap();

        // 第二次：记着被控端的公钥，一个往返（IK）
        let (c, h) = run(&gate, &me, Some(*gate.me.public()), None).await;
        let (c, h) = (c.unwrap(), h.unwrap());
        assert_eq!(c.secured.safety, h.secured.safety);
        assert_eq!(h.secured.peer, *me.public());
    }

    /// 被控端的公钥和记着的不一样：不连，也不能不验证地换成新的；输对配对码才认新的身份
    #[tokio::test]
    async fn a_different_host_key_needs_pairing_again() {
        let gate = TestGate::new(Access::Open, Some("123456"));
        let me = Identity::generate();
        let stale = *Identity::generate().public();
        let (c, h) = run(&gate, &me, Some(stale), None).await;
        assert_eq!(problem(c), Problem::HostKeyChanged);
        assert!(h.is_err());
        let (c, _) = run(&gate, &me, Some(stale), Some("000000")).await;
        assert_eq!(problem(c), Problem::PairingFailed);
        let (c, h) = run(&gate, &me, Some(stale), Some("123456")).await;
        assert_eq!(c.unwrap().secured.peer, *gate.me.public());
        assert_eq!(h.unwrap().admission, Admission::Paired);
    }

    #[tokio::test]
    async fn pairing() {
        let gate = TestGate::new(Access::Pairing, Some("482913"));
        let me = Identity::generate();
        // 没配对过：被控端要求配对码
        let (c, h) = run(&gate, &me, None, None).await;
        assert_eq!(problem(c), Problem::PairingRequired);
        assert!(h.is_err());
        // 配对码不对：失败，被控端记一次
        let (c, h) = run(&gate, &me, None, Some("000000")).await;
        assert_eq!(problem(c), Problem::PairingFailed);
        assert!(h.is_err());
        assert_eq!(gate.failures.load(Ordering::Relaxed), 1);
        // 对了：配对成功，之后不用配对码
        let (c, h) = run(&gate, &me, None, Some("482913")).await;
        let (c, h) = (c.unwrap(), h.unwrap());
        assert_eq!(h.admission, Admission::Paired);
        assert_eq!(c.secured.safety, h.secured.safety);
        let (c, h) = run(&gate, &me, Some(c.secured.peer), None).await;
        assert!(c.is_ok());
        assert_eq!(h.unwrap().admission, Admission::Known);
        // 别的控制端照样要配对
        let (c, _) = run(&gate, &Identity::generate(), Some(*gate.me.public()), None).await;
        assert_eq!(problem(c), Problem::PairingRequired);
        // 暂停配对了
        *gate.code.lock().unwrap() = None;
        let (c, _) = run(&gate, &Identity::generate(), None, Some("482913")).await;
        assert_eq!(problem(c), Problem::PairingUnavailable);
    }

    /// 中转把录下来的 IK 第 1 条消息重放给被控端：被控端照样回答（它分不出来），但重放的一方拿不出确认，
    /// 被控端不会当成那个配对过的控制端进来了（不显示、不请本机用户同意）
    #[tokio::test]
    async fn a_replayed_first_message_cannot_confirm() {
        let gate = TestGate::new(Access::Pairing, None);
        let me = Identity::generate();
        gate.known.lock().unwrap().insert(*me.public());
        // 一次真的握手，录下控制端的第 1 条消息
        let intro = test_intro();
        let (initiator, first) =
            initiate(params(&me, Some(*gate.me.public()), None, &intro)).unwrap();
        let ((mut cr, mut cw), (hr, mut hw)) = pipes();
        let (c, h) = tokio::join!(
            initiator.finish(&mut cr, &mut cw),
            respond(&first, &mut hw, async { Ok(hr) }, responder(&gate))
        );
        assert!(c.is_ok());
        assert_eq!(h.unwrap().0.admission, Admission::Known);

        // 重放：被控端回答了（它分不出来），但重放的一方算不出会话密钥，随便发一帧都解不开
        let ((mut fake_r, mut fake_w), (mut hr, mut hw)) = pipes();
        let (r, _) = respond(&first, &mut hw, async { Ok(&mut hr) }, responder(&gate))
            .await
            .unwrap();
        assert_eq!(r.admission, Admission::Known);
        let mut buf = Vec::new();
        read_frame(&mut fake_r, 1 << 16, &mut buf).await.unwrap();
        let reply: HandshakeReply = framing::decode_known(&buf).unwrap();
        assert!(reply.verdict.is_some() && reply.error.is_none());
        write_frame(&mut fake_w, &[0u8; 40]).await.unwrap();
        read_frame(&mut hr, 1 << 16, &mut buf).await.unwrap();
        let mut opener = r.opener;
        assert!(opener.open(&mut buf).is_err());
    }

    /// 被控端不会控制端用的模式（以后的版本加的）：回答它会哪些，控制端据此判断该更新哪一边
    #[tokio::test]
    async fn an_unknown_pattern_is_answered_with_the_known_ones() {
        let gate = TestGate::new(Access::Open, None);
        let ((mut cr, _cw), (hr, mut hw)) = pipes();
        let init = framing::to_vec(&HandshakeInit {
            pattern: Pattern::Unknown(7),
            pake: None,
            noise: vec![1, 2, 3],
        });
        let h = respond(&init, &mut hw, async { Ok(hr) }, responder(&gate)).await;
        assert!(h.is_err());
        let mut buf = Vec::new();
        read_frame(&mut cr, 1 << 16, &mut buf).await.unwrap();
        let reply: HandshakeReply = framing::decode_known(&buf).unwrap();
        assert_eq!(reply.error, Some(HandshakeError::Unsupported));
        let theirs = reply.patterns.unwrap();
        assert_eq!(theirs, Pattern::KNOWN);
        // 本机用的比对方会的都新：对方该更新
        let p = handshake_problem(
            HandshakeError::Unsupported,
            Pattern::Unknown(7),
            Some(&theirs[..]),
        );
        assert!(matches!(p, Problem::PeerIncompatible { peer, ours } if peer < ours));
        // 对方会更新的：本机该更新
        let p = handshake_problem(
            HandshakeError::Unsupported,
            Pattern::Known,
            Some(&[Pattern::Unknown(9)][..]),
        );
        assert!(matches!(p, Problem::PeerIncompatible { peer, ours } if peer > ours));
        // 太大的、解不出来的第一条消息：回答格式不对
        for bad in [vec![0xff; 3], vec![0x40; MAX_MSG + 1]] {
            let ((mut cr, _cw), (hr, mut hw)) = pipes();
            assert!(
                respond(&bad, &mut hw, async { Ok(hr) }, responder(&gate))
                    .await
                    .is_err()
            );
            read_frame(&mut cr, 1 << 16, &mut buf).await.unwrap();
            let reply: HandshakeReply = framing::decode_known(&buf).unwrap();
            assert_eq!(reply.error, Some(HandshakeError::Malformed));
        }
    }

    #[tokio::test]
    async fn approval_is_left_to_the_session() {
        let gate = TestGate::new(Access::Approval, None);
        let (c, h) = run(&gate, &Identity::generate(), None, None).await;
        assert!(c.is_ok());
        assert_eq!(h.unwrap().admission, Admission::NeedsApproval);
    }

    /// 中间人：篡改握手消息，连不上
    #[tokio::test]
    async fn tampering_is_detected() {
        let gate = TestGate::new(Access::Open, None);
        let me = Identity::generate();
        let intro = test_intro();
        let (initiator, init) = initiate(params(&me, None, None, &intro)).unwrap();
        // 把控制端的第一条消息里的 Noise 消息改一个字节
        let mut tampered: HandshakeInit = framing::decode_known(&init).unwrap();
        let last = tampered.noise.len() - 1;
        tampered.noise[last] ^= 1;
        let tampered = framing::to_vec(&tampered);
        let ((mut cr, mut cw), (hr, mut hw)) = pipes();
        let (c, h) = tokio::time::timeout(Duration::from_secs(5), async {
            // 各自拿着自己那一端：控制端失败时断开，被控端随之失败
            tokio::join!(
                async move { initiator.finish(&mut cr, &mut cw).await },
                async move {
                    let r = respond(&tampered, &mut hw, async { Ok(hr) }, responder(&gate)).await;
                    // 被控端失败之后不会再发什么；断开，控制端随之失败
                    drop(hw);
                    r
                }
            )
        })
        .await
        .unwrap();
        assert!(c.is_err() && h.is_err());
    }

    /// 两边的密钥（不经过网络）
    fn materials() -> (KeyMaterial, KeyMaterial) {
        let a = Identity::generate();
        let b = Identity::generate();
        let mut i = snow::Builder::new(noise(Pattern::First).unwrap())
            .local_private_key(a.private())
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut r = snow::Builder::new(noise(Pattern::First).unwrap())
            .local_private_key(b.private())
            .unwrap()
            .build_responder()
            .unwrap();
        let mut m = [0u8; 1024];
        let mut p = [0u8; 1024];
        let n = i.write_message(&[], &mut m).unwrap();
        r.read_message(&m[..n], &mut p).unwrap();
        let n = r.write_message(&[], &mut m).unwrap();
        i.read_message(&m[..n], &mut p).unwrap();
        let n = i.write_message(&[], &mut m).unwrap();
        r.read_message(&m[..n], &mut p).unwrap();
        (
            KeyMaterial::new(Role::Controller, &mut i).unwrap(),
            KeyMaterial::new(Role::Host, &mut r).unwrap(),
        )
    }

    #[test]
    fn frames_fail_when_dropped_reordered_or_moved() {
        let (ctl, host) = materials();
        let control = |m: &KeyMaterial| m.keys(label(SessionLane::Control), Order::Next(0));
        let frames: Vec<Vec<u8>> = {
            let mut s = control(&ctl).sealer;
            (0..3u8)
                .map(|k| {
                    let mut f = framing::encode_with_spare(
                        &airhand_proto::session::ControllerMsg::RequestKeyframe { after: k as u32 },
                        SEAL_OVERHEAD,
                    );
                    let cap = f.capacity();
                    s.seal_frame(&mut f).unwrap();
                    assert_eq!(f.capacity(), cap, "sealing reallocated");
                    // 长度前缀之后：密文 | 标签 | 计数
                    assert_eq!(f[f.len() - NONCE_LEN..], u64::from(k).to_le_bytes());
                    f[4..].to_vec()
                })
                .collect()
        };
        // 按顺序：都解得开
        let mut o = control(&host).opener;
        for f in &frames {
            o.open(&mut f.clone()).unwrap();
        }
        // 丢了一帧、重放
        let mut o = control(&host).opener;
        o.open(&mut frames[0].clone()).unwrap();
        assert!(o.open(&mut frames[2].clone()).is_err());
        assert!(o.open(&mut frames[0].clone()).is_err());
        // 改了计数（nonce 不对，解不开）
        let mut forged = frames[2].clone();
        let at = forged.len() - NONCE_LEN;
        forged[at] = 1;
        assert!(o.open(&mut forged).is_err());
        o.open(&mut frames[1].clone()).unwrap();
        // 换到别的通道
        let mut o = host.keys(label(SessionLane::Bulk), Order::Next(0)).opener;
        assert!(o.open(&mut frames[0].clone()).is_err());
        // 方向反了（被控端自己发的不能当成收到的）
        let back = control(&host).sealer.seal(b"x").unwrap();
        let mut o = control(&host).opener;
        assert!(o.open(&mut back.clone()).is_err());
        assert!(o.open(&mut vec![0; SEAL_OVERHEAD - 1]).is_err());

        // 声音数据报：乱序可以，重放、篡改不行
        let (ctl_keys, _) = ctl.split();
        let (host_keys, _) = host.split();
        let mut s = host_keys.audio.sealer;
        let mut o = ctl_keys.audio.opener;
        let d: Vec<Vec<u8>> = (0..4u8)
            .map(|k| {
                s.seal_datagram(7, airhand_proto::datagram::AUDIO, &[k; 10])
                    .unwrap()
            })
            .collect();
        assert_eq!(airhand_proto::datagram::session(&d[0]), Some(7));
        assert_eq!(
            airhand_proto::datagram::kind(&d[0]),
            Some(airhand_proto::datagram::AUDIO)
        );
        let body = |d: &Vec<u8>| d[airhand_proto::datagram::HEADER..].to_vec();
        // 计数在末尾，不用解密就看得到
        assert_eq!(counter_of(&body(&d[2])), Some(2));
        assert_eq!(counter_of(&[0; SEAL_OVERHEAD - 1]), None);
        let open = |o: &mut Opener, mut b: Vec<u8>| o.open(&mut b).map(|()| b);
        assert_eq!(open(&mut o, body(&d[1])).unwrap(), [1; 10]);
        assert_eq!(open(&mut o, body(&d[0])).unwrap(), [0; 10]);
        assert!(open(&mut o, body(&d[1])).is_err());
        let mut bad = body(&d[3]);
        bad[2] ^= 1;
        assert!(open(&mut o, bad).is_err());
        assert_eq!(open(&mut o, body(&d[3])).unwrap(), [3; 10]);
        assert_eq!(open(&mut o, body(&d[2])).unwrap(), [2; 10]);
        // 探测、控制数据报和声音的密钥都不一样
        let mut p = host_keys.probe.sealer;
        let probe = p.seal(&[1]).unwrap();
        assert!(open(&mut o, probe).is_err());
        let mut c = host_keys.datagrams.sealer;
        let control = c.seal(&[1]).unwrap();
        assert!(open(&mut o, control.clone()).is_err());
        let mut o = ctl_keys.datagrams.opener;
        assert_eq!(open(&mut o, control.clone()).unwrap(), [1]);
        // 直连通着时同一个数据报两条路各来一份：后到的是重放
        assert!(open(&mut o, control).is_err());
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        for c in [5, 3, 200, 199, 73] {
            assert!(w.fresh(c), "{c}");
            w.accept(c);
            assert!(!w.fresh(c), "{c}");
        }
        // 太老了（窗口之外）
        assert!(!w.fresh(5));
        assert!(w.fresh(198));
        assert!(w.fresh(1000));
    }

    #[test]
    fn identity_roundtrip() {
        let a = Identity::generate();
        let b = Identity::from_private(*a.private());
        assert_eq!(a.public(), b.public());
        assert_eq!(unhex32(&hex(a.public())), Some(*a.public()));
        assert_eq!(unhex32("zz"), None);
    }
}
