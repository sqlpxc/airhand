//! 端到端加密（协议见 `airhand_proto::handshake`）：设备的身份密钥、控制通道上的握手、之后每一帧的加解密。
//!
//! * [`Identity`]：一台设备的长期 X25519 密钥（Noise 的静态密钥），由各端自己保存；
//! * [`initiate`] / [`respond`]：控制端 / 被控端在控制通道上握手（最后是控制端的确认），得到 [`Secured`]
//!   和双方协商好的 [`Negotiated`]；
//! * [`Secured`]：派生出每条通道、每个方向的密钥：[`FrameSealer`] / [`FrameOpener`]（流上的帧）、
//!   [`DatagramSealer`] / [`DatagramOpener`]（声音数据报）。
//!
//! 加解密都是原地做的：发送的帧预留了认证标签的位置（`airhand_proto::framing::encode_with_spare`、
//! `airhand_proto::media::frame`），收到的帧解密之后截掉标签，画面包照样原样交给界面，不多拷贝。

use std::time::Duration;

use crate::wire::{read_frame, write_frame};
use airhand_proto::framing;
use airhand_proto::handshake::{
    self, Confirm, ControllerIntro, HandshakeError, HandshakeFinish, HandshakeInit, HandshakeReply,
    HostVerdict, PAKE_CONTROLLER, PAKE_HOST, Pattern, PublicKey, Refusal, TAG_LEN,
};
use airhand_proto::session::QualitySettings;
use airhand_proto::{Role, SESSION_MIN, SESSION_VERSION, SessionLane};
use anyhow::{Context, Result, anyhow, bail};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use spake2::{Ed25519Group, Identity as PakeId, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

pub use airhand_proto::handshake::Negotiated;

use crate::Problem;

/// 握手的消息都很小
const MAX_HANDSHAKE_MSG: usize = 16 * 1024;
/// 握手的每一步最多等这么久（被控端的用户同意不在握手里，在之后的会话里）
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

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
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, raw).expect("32-byte key"))
}

/// 一条通道一个方向的发送端：每一帧用下一个计数做 nonce（计数不上线，收的一方按顺序数）
pub struct FrameSealer {
    key: LessSafeKey,
    counter: u64,
}

impl FrameSealer {
    /// 把一帧（`u32 长度 | 明文`，`framing::encode` / `media::frame` 编好的）原地加密成 `u32 长度 | 密文 | 标签`
    pub fn seal_frame(&mut self, frame: &mut Vec<u8>) -> Result<()> {
        anyhow::ensure!(frame.len() >= 4, "not a frame");
        let n = self.next()?;
        let tag = self
            .key
            .seal_in_place_separate_tag(n, Aad::empty(), &mut frame[4..])
            .map_err(|_| anyhow!("encryption failed"))?;
        frame.extend_from_slice(tag.as_ref());
        let len = (frame.len() - 4) as u32;
        frame[..4].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }

    /// 加密一段负载（不带长度前缀），返回 `密文 | 标签`
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(plaintext.len() + TAG_LEN);
        out.extend_from_slice(plaintext);
        let n = self.next()?;
        self.key
            .seal_in_place_append_tag(n, Aad::empty(), &mut out)
            .map_err(|_| anyhow!("encryption failed"))?;
        Ok(out)
    }

    fn next(&mut self) -> Result<Nonce> {
        let c = self.counter;
        self.counter = c.checked_add(1).context("nonce counter exhausted")?;
        Ok(nonce(c))
    }
}

/// 一条通道一个方向的接收端
pub struct FrameOpener {
    key: LessSafeKey,
    counter: u64,
}

impl FrameOpener {
    /// 原地解密一帧的负载（`密文 | 标签`，不带长度前缀），之后 `payload` 就是明文。
    /// 解不开（被改过、丢了一帧、顺序不对、不是这条通道的）是错误：会话不能再继续
    pub fn open(&mut self, payload: &mut Vec<u8>) -> Result<()> {
        let c = self.counter;
        self.counter = c.checked_add(1).context("nonce counter exhausted")?;
        let len = self
            .key
            .open_in_place(nonce(c), Aad::empty(), payload)
            .map_err(|_| anyhow!("a session frame failed authentication"))?
            .len();
        payload.truncate(len);
        Ok(())
    }
}

/// 声音数据报的发送端：`会话 ID | u64 计数 | 密文 | 标签`
pub struct DatagramSealer {
    key: LessSafeKey,
    counter: u64,
}

/// 数据报在会话 ID 之后的开销：计数 + 标签
pub const DATAGRAM_OVERHEAD: usize = 8 + TAG_LEN;

impl DatagramSealer {
    pub fn seal(&mut self, session_id: u64, packet: &[u8]) -> Result<Vec<u8>> {
        let c = self.counter;
        self.counter = c.checked_add(1).context("nonce counter exhausted")?;
        let mut d =
            Vec::with_capacity(airhand_proto::datagram::HEADER + DATAGRAM_OVERHEAD + packet.len());
        d.extend_from_slice(&session_id.to_le_bytes());
        d.extend_from_slice(&c.to_le_bytes());
        let body = d.len();
        d.extend_from_slice(packet);
        let tag = self
            .key
            .seal_in_place_separate_tag(nonce(c), Aad::empty(), &mut d[body..])
            .map_err(|_| anyhow!("encryption failed"))?;
        d.extend_from_slice(tag.as_ref());
        Ok(d)
    }
}

/// 声音数据报的接收端：数据报可能丢、可能乱序，计数是明文；用滑动窗口挡掉重放的
pub struct DatagramOpener {
    key: LessSafeKey,
    window: ReplayWindow,
}

impl DatagramOpener {
    /// `body`：会话 ID 后面的部分。解不开、重放的返回 `None`（丢掉，数据报本来就可能丢）
    pub fn open(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        let counter = u64::from_le_bytes(body.get(..8)?.try_into().ok()?);
        if !self.window.fresh(counter) {
            return None;
        }
        let mut buf = body[8..].to_vec();
        let len = self
            .key
            .open_in_place(nonce(counter), Aad::empty(), &mut buf)
            .ok()?
            .len();
        buf.truncate(len);
        // 认证通过之后才记下：伪造的数据报不能把窗口推走
        self.window.accept(counter);
        Some(buf)
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
            let shift = c + 1 - self.top;
            self.seen = if shift >= 128 { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.top = c + 1;
        } else {
            self.seen |= 1 << (self.top - 1 - c);
        }
    }
}

// ───────────────────────────── 会话的密钥 ─────────────────────────────

/// 握手的结果：会话的密钥（各通道、各方向从这里派生），对方是谁，安全码
pub struct Secured {
    /// 本端是哪一边（控制端是 Noise 的发起方）
    role: Role,
    /// 发起方 → 响应方、响应方 → 发起方
    i2r: [u8; 32],
    r2i: [u8; 32],
    /// 握手的哈希（派生用的 salt）
    h: [u8; 32],
    /// 对方的公钥
    pub peer: PublicKey,
    /// 安全码（两端一样，见 `airhand_proto::handshake::safety_code`）
    pub safety: String,
}

impl Drop for Secured {
    fn drop(&mut self) {
        self.i2r = [0; 32];
        self.r2i = [0; 32];
    }
}

impl Secured {
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

    fn key(&self, sending: bool, label: &[u8]) -> LessSafeKey {
        // 控制端发送 = 发起方 → 响应方
        let i2r = sending == (self.role == Role::Controller);
        let ikm = if i2r { &self.i2r } else { &self.r2i };
        let mut out = [0u8; 32];
        derive(&self.h, ikm, label, &mut out);
        aead_key(&out)
    }

    pub fn sealer(&self, lane: SessionLane) -> FrameSealer {
        FrameSealer {
            key: self.key(true, &lane_label(lane)),
            counter: 0,
        }
    }

    pub fn opener(&self, lane: SessionLane) -> FrameOpener {
        FrameOpener {
            key: self.key(false, &lane_label(lane)),
            counter: 0,
        }
    }

    pub fn datagram_sealer(&self) -> DatagramSealer {
        DatagramSealer {
            key: self.key(true, b"airhand datagram"),
            counter: 0,
        }
    }

    pub fn datagram_opener(&self) -> DatagramOpener {
        DatagramOpener {
            key: self.key(false, b"airhand datagram"),
            window: ReplayWindow::default(),
        }
    }
}

/// 每条通道的密钥用不同的标签派生（通道编号）：从别的通道搬过来的帧解不开
fn lane_label(lane: SessionLane) -> [u8; 13] {
    let mut l = *b"airhand lane ";
    l[12] = b'0' + lane as u8;
    l
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
    let got = tokio::time::timeout(STEP_TIMEOUT, read_frame(r, MAX_HANDSHAKE_MSG, buf))
        .await
        .context("the end-to-end handshake timed out")??;
    anyhow::ensure!(got, "the session closed during the end-to-end handshake");
    Ok(())
}

async fn send_step<W: AsyncWrite + Unpin, T: airhand_proto::minicbor::Encode<()>>(
    w: &mut W,
    msg: &T,
) -> Result<()> {
    write_frame(w, &framing::to_vec(msg)).await?;
    Ok(())
}

/// 控制端握手的结果
pub struct Initiated {
    pub secured: Secured,
    /// 控制通道上被控端 → 控制端的接收端（已经解开了被控端的决定，计数接着往下数）
    pub opener: FrameOpener,
    /// 控制通道上控制端 → 被控端的发送端（已经发出了确认，计数接着往下数）
    pub sealer: FrameSealer,
    /// 选定的会话版本、双方都支持的能力位（和被控端算出来的一样）
    pub negotiated: Negotiated,
}

/// 控制端怎么证明自己、怎么认被控端
pub struct InitiatorParams<'a> {
    pub session_id: u64,
    /// 控制码（配对时算进 SPAKE2 的密码）
    pub code: u32,
    pub me: &'a Identity,
    /// 之前记下的被控端公钥（"我的设备"）：只认它
    pub host_key: Option<PublicKey>,
    /// 配对码（被控端要求配对时用户输入的）
    pub pairing: Option<&'a str>,
    pub intro: &'a ControllerIntro,
}

/// 控制端：在控制通道上握手（见 `airhand_proto::handshake`）。失败时的错误是 [`Problem`]
/// （对方身份变了、要配对码、配对码不对、版本不兼容……）或者普通的网络错误
pub async fn initiate<R, W>(recv: &mut R, send: &mut W, p: InitiatorParams<'_>) -> Result<Initiated>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let pattern = match (p.pairing, p.host_key) {
        (Some(_), _) => Pattern::Pairing,
        (None, Some(_)) => Pattern::Known,
        (None, None) => Pattern::First,
    };
    let pake = p.pairing.map(|code| {
        Spake2::<Ed25519Group>::start_a(
            &Password::new(handshake::pake_password(p.session_id, p.code, code)),
            &PakeId::new(PAKE_CONTROLLER),
            &PakeId::new(PAKE_HOST),
        )
    });
    let mut init = HandshakeInit {
        pattern,
        pake: pake.as_ref().map(|(_, msg)| msg.clone()),
        noise: Vec::new(),
    };
    let prologue = handshake::prologue(p.session_id, &init);
    let mut builder = snow::Builder::new(noise(pattern)?)
        .local_private_key(p.me.private())?
        .prologue(&prologue)?;
    if let (Pattern::Known, Some(k)) = (pattern, &p.host_key) {
        builder = builder.remote_public_key(k)?;
    }
    let mut hs = builder.build_initiator()?;
    let intro = framing::to_vec(p.intro);
    let mut msg = vec![0u8; 65535];
    // IK：介绍放在第 1 条里（加密、认证过）；XX：第 1 条什么都不能带
    let n = hs.write_message(
        if pattern == Pattern::Known {
            &intro
        } else {
            &[]
        },
        &mut msg,
    )?;
    init.noise = msg[..n].to_vec();
    send_step(send, &init).await?;

    let mut buf = Vec::new();
    read_step(recv, &mut buf).await?;
    let reply: HandshakeReply = framing::decode_known(&buf)?;
    if let Some(e) = reply.error {
        return Err(handshake_problem(e, pattern, reply.patterns.as_deref()).into());
    }
    if let Some((spake, _)) = pake {
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
    let mut payload = vec![0u8; 65535];
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
        // 已经知道被控端的公钥了：配对时如果之前记着它，要一样
        let theirs: PublicKey = hs
            .get_remote_static()
            .context("the host sent no static key")?
            .try_into()?;
        if p.host_key.is_some_and(|k| k != theirs) {
            return Err(Problem::HostKeyChanged.into());
        }
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
    let secured = Secured::new(Role::Controller, &mut hs)?;
    let mut opener = secured.opener(SessionLane::Control);
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
            return Err(Problem::failed(format!("the host refused the connection ({r:?})")).into());
        }
    };
    anyhow::ensure!(
        (SESSION_MIN..=SESSION_VERSION).contains(&version),
        "the host chose session version {version}, which we can't speak"
    );
    let negotiated = Negotiated::new(version, verdict.features);
    // 确认（见 airhand_proto::handshake 的"确认"）：被控端解开它才算本机进来了，重放的握手消息走不到这一步
    let mut sealer = secured.sealer(SessionLane::Control);
    let mut confirm = framing::encode_with_spare(&Confirm::default(), TAG_LEN);
    sealer.seal_frame(&mut confirm)?;
    send.write_all(&confirm).await?;
    debug!(?pattern, ?negotiated, safety = %secured.safety, "end-to-end handshake done");
    Ok(Initiated {
        negotiated,
        secured,
        opener,
        sealer,
    })
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

/// 被控端：自己是谁，认得哪些控制端，配对码是什么（各平台自己保存）
pub trait Gate: Send + Sync + 'static {
    fn identity(&self) -> Identity;
    fn access(&self) -> Access;
    /// 现在的配对码；`None` 表示不接受配对（没有设置，或者输错太多次、暂停了）
    fn pairing_code(&self) -> Option<String>;
    /// 有人输错了配对码（连续错几次就暂停配对，要本机用户换一个配对码）
    fn pairing_failed(&self);
    /// 这个控制端配对过（或者本机用户这次运行里同意过）
    fn is_known(&self, key: &PublicKey) -> bool;
    /// 配对成功：记住这个控制端
    fn paired(&self, key: PublicKey, name: &str);
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

/// 被控端握手的结果：控制端已经确认过了（见 `airhand_proto::handshake` 的"确认"），是真的在连
pub struct Responded {
    pub secured: Secured,
    /// 控制通道上被控端 → 控制端的发送端（已经发出了决定，计数接着往下数）
    pub sealer: FrameSealer,
    /// 控制通道上控制端 → 被控端的接收端（已经解开了确认，计数接着往下数）
    pub opener: FrameOpener,
    pub intro: ControllerIntro,
    pub admission: Admission,
    /// 选定的会话版本、双方都支持的能力位（和控制端算出来的一样）
    pub negotiated: Negotiated,
}

pub struct ResponderParams<'a> {
    pub session_id: u64,
    /// 本机的控制码（配对时算进 SPAKE2 的密码）
    pub code: u32,
    pub gate: &'a dyn Gate,
}

/// 被控端：在控制通道上握手（见 `airhand_proto::handshake`）。让不让进来由 [`Gate`] 决定；
/// 拒绝时已经把原因告诉了控制端，返回错误。
///
/// 收到控制端的确认才返回：在那之前不能显示"谁在连"、不能开始采集（`Noise_IK` 的第 1 条消息可以被重放）
pub async fn respond<R, W>(recv: &mut R, send: &mut W, p: ResponderParams<'_>) -> Result<Responded>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::new();
    read_step(recv, &mut buf).await?;
    let init: HandshakeInit = match framing::decode_known(&buf) {
        Ok(i) => i,
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
    if pattern == Pattern::Pairing {
        let Some(code) = p.gate.pairing_code() else {
            fail(send, HandshakeError::PairingUnavailable).await;
            bail!("a controller tried to pair, but pairing is off");
        };
        let (spake, msg_b) = Spake2::<Ed25519Group>::start_b(
            &Password::new(handshake::pake_password(p.session_id, p.code, &code)),
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
    let prologue = handshake::prologue(p.session_id, &init);
    let mut hs = snow::Builder::new(noise(pattern)?)
        .local_private_key(me.private())?
        .prologue(&prologue)?
        .build_responder()?;
    if let Some(psk) = &psk {
        hs.set_psk(3, psk)?;
    }
    let mut payload = vec![0u8; 65535];
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
    let mut msg = vec![0u8; 65535];
    let m = hs.write_message(&[], &mut msg)?;
    reply.noise = Some(msg[..m].to_vec());

    let intro_bytes = if pattern == Pattern::Known {
        payload[..n].to_vec()
    } else {
        send_step(send, &reply).await?;
        reply = HandshakeReply::default();
        read_step(recv, &mut buf).await?;
        let finish: HandshakeFinish = framing::decode_known(&buf)?;
        match hs.read_message(&finish.noise, &mut payload) {
            Ok(n) => payload[..n].to_vec(),
            Err(_) if pattern == Pattern::Pairing => {
                p.gate.pairing_failed();
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
    let intro: ControllerIntro = framing::decode_known(&intro_bytes)?;
    let secured = Secured::new(Role::Host, &mut hs)?;

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
    let features = airhand_proto::features::session::SUPPORTED;
    let refused = if !compatible {
        Some(Refusal::Incompatible)
    } else if admission.is_none() {
        Some(Refusal::PairingRequired)
    } else {
        None
    };
    let verdict = HostVerdict {
        version: refused.is_none().then_some(version),
        features,
        refused,
        min: SESSION_MIN,
        max: SESSION_VERSION,
    };
    let mut sealer = secured.sealer(SessionLane::Control);
    reply.verdict = Some(sealer.seal(&framing::to_vec(&verdict))?);
    send_step(send, &reply).await?;
    if let Some(r) = refused {
        bail!("refused the controller: {r:?}");
    }
    let admission = admission.expect("checked above");

    // 等控制端的确认（见 airhand_proto::handshake 的"确认"）：解得开它才算这个控制端真的在连。
    // 重放的第 1 条消息走不到这里（重放的一方算不出会话密钥），在这之前什么也不记、什么也不显示
    let mut opener = secured.opener(SessionLane::Control);
    read_step(recv, &mut buf)
        .await
        .context("waiting for the controller to confirm the handshake")?;
    opener
        .open(&mut buf)
        .context("the controller's confirmation failed authentication")?;
    let _: Confirm = framing::decode_known(&buf)?;

    if admission == Admission::Paired {
        info!(name = %intro.name, key = %hex(&secured.peer), "a controller paired with this host");
        p.gate.paired(secured.peer, &intro.name);
    }
    Ok(Responded {
        negotiated,
        secured,
        sealer,
        opener,
        intro,
        admission,
    })
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
        fn pairing_code(&self) -> Option<String> {
            self.code.lock().unwrap().clone()
        }
        fn pairing_failed(&self) {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        fn is_known(&self, key: &PublicKey) -> bool {
            self.known.lock().unwrap().contains(key)
        }
        fn paired(&self, key: PublicKey, _: &str) {
            self.known.lock().unwrap().insert(key);
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

    /// 握手一次。返回控制端和被控端各自的结果
    async fn run(
        gate: &TestGate,
        me: &Identity,
        host_key: Option<PublicKey>,
        pairing: Option<&str>,
    ) -> (Result<Initiated>, Result<Responded>) {
        let ((mut cr, mut cw), (mut hr, mut hw)) = pipes();
        let intro = test_intro();
        // 各自拿着自己那一端：一边结束（失败）时连接随之关闭，另一边马上知道
        let c = async move {
            initiate(
                &mut cr,
                &mut cw,
                InitiatorParams {
                    session_id: 42,
                    code: 123_456,
                    me,
                    host_key,
                    pairing,
                    intro: &intro,
                },
            )
            .await
        };
        let h = async move {
            respond(
                &mut hr,
                &mut hw,
                ResponderParams {
                    session_id: 42,
                    code: 123_456,
                    gate,
                },
            )
            .await
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

        // 两边的密钥对得上：控制端 → 被控端、被控端 → 控制端，每条通道
        let mut c_send = c.secured.sealer(SessionLane::Video);
        let mut h_recv = h.secured.opener(SessionLane::Video);
        let mut frame = airhand_proto::media::frame(&[7; 24], &[b"hello"]);
        let cap = frame.capacity();
        c_send.seal_frame(&mut frame).unwrap();
        assert_eq!(frame.capacity(), cap, "sealing reallocated");
        let mut payload = frame[4..].to_vec();
        h_recv.open(&mut payload).unwrap();
        assert_eq!(&payload[24..], b"hello");
        // 控制通道：被控端的发送端接着决定往下数，控制端的发送端接着确认往下数
        let (mut h_send, mut c_recv) = (h.sealer, c.opener);
        let mut f = framing::encode(&airhand_proto::session::HostMsg::Pong { t: 1 });
        h_send.seal_frame(&mut f).unwrap();
        let mut p = f[4..].to_vec();
        c_recv.open(&mut p).unwrap();
        let (mut c_send, mut h_recv) = (c.sealer, h.opener);
        let mut f = framing::encode(&airhand_proto::session::ControllerMsg::Ping { t: 1 });
        c_send.seal_frame(&mut f).unwrap();
        let mut p = f[4..].to_vec();
        h_recv.open(&mut p).unwrap();

        // 第二次：记着被控端的公钥，一个往返（IK）
        let (c, h) = run(&gate, &me, Some(*gate.me.public()), None).await;
        let (c, h) = (c.unwrap(), h.unwrap());
        assert_eq!(c.secured.safety, h.secured.safety);
        assert_eq!(h.secured.peer, *me.public());
    }

    #[tokio::test]
    async fn a_different_host_key_is_refused() {
        let gate = TestGate::new(Access::Open, Some("123456"));
        let me = Identity::generate();
        let stale = *Identity::generate().public();
        let (c, h) = run(&gate, &me, Some(stale), None).await;
        assert_eq!(problem(c), Problem::HostKeyChanged);
        assert!(h.is_err());
        // 配对时也一样：被控端的公钥和记着的不一样就不连
        let (c, _) = run(&gate, &me, Some(stale), Some("123456")).await;
        assert_eq!(problem(c), Problem::HostKeyChanged);
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

    /// 中转把录下来的 IK 第 1 条消息重放给被控端：被控端照样回答（它分不出来），但等不到确认，
    /// 不会当成那个配对过的控制端进来了（不显示、不采集）
    #[tokio::test]
    async fn a_replayed_first_message_is_not_admitted() {
        let gate = TestGate::new(Access::Pairing, None);
        let me = Identity::generate();
        gate.paired(*me.public(), "ctl");
        // 录下一次真的握手里控制端的第 1 条消息
        let ((mut cr, mut cw), (mut mr, mut mw)) = pipes();
        let ((mut hr, mut hw), (mut nr, mut nw)) = pipes();
        let intro = test_intro();
        let ctl = initiate(
            &mut cr,
            &mut cw,
            InitiatorParams {
                session_id: 5,
                code: 123_456,
                me: &me,
                host_key: Some(*gate.me.public()),
                pairing: None,
                intro: &intro,
            },
        );
        let host = respond(
            &mut nr,
            &mut nw,
            ResponderParams {
                session_id: 5,
                code: 123_456,
                gate: &gate,
            },
        );
        let relay = async move {
            let mut first = Vec::new();
            read_frame(&mut mr, 1 << 16, &mut first).await.unwrap();
            write_frame(&mut hw, &first).await.unwrap();
            let mut buf = Vec::new();
            read_frame(&mut hr, 1 << 16, &mut buf).await.unwrap();
            write_frame(&mut mw, &buf).await.unwrap();
            read_frame(&mut mr, 1 << 16, &mut buf).await.unwrap();
            write_frame(&mut hw, &buf).await.unwrap();
            first
        };
        let (c, h, first) = tokio::join!(ctl, host, relay);
        assert!(c.is_ok());
        assert_eq!(h.unwrap().admission, Admission::Known);

        // 重放：被控端回答了，但重放的一方拿不出确认（随便发一帧、或者干脆不发）
        let ((mut hr, mut hw), (mut nr, mut nw)) = pipes();
        let host = respond(
            &mut nr,
            &mut nw,
            ResponderParams {
                session_id: 5,
                code: 123_456,
                gate: &gate,
            },
        );
        let replay = async move {
            write_frame(&mut hw, &first).await.unwrap();
            let mut buf = Vec::new();
            read_frame(&mut hr, 1 << 16, &mut buf).await.unwrap();
            let reply: HandshakeReply = framing::decode_known(&buf).unwrap();
            assert!(reply.verdict.is_some() && reply.error.is_none());
            write_frame(&mut hw, &[0u8; 40]).await.unwrap();
        };
        let (h, ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(host, replay) })
                .await
                .unwrap();
        let e = h.err().expect("a replayed handshake must not be admitted");
        assert!(format!("{e:#}").contains("confirmation"), "{e:#}");
    }

    /// 被控端不会控制端用的模式（以后的版本加的）：回答它会哪些，控制端据此判断该更新哪一边
    #[tokio::test]
    async fn an_unknown_pattern_is_answered_with_the_known_ones() {
        let gate = TestGate::new(Access::Open, None);
        let ((mut hr, mut hw), (mut nr, mut nw)) = pipes();
        let host = respond(
            &mut nr,
            &mut nw,
            ResponderParams {
                session_id: 1,
                code: 123_456,
                gate: &gate,
            },
        );
        let ctl = async move {
            let init = HandshakeInit {
                pattern: Pattern::Unknown(7),
                pake: None,
                noise: vec![1, 2, 3],
            };
            write_frame(&mut hw, &framing::to_vec(&init)).await.unwrap();
            let mut buf = Vec::new();
            read_frame(&mut hr, 1 << 16, &mut buf).await.unwrap();
            framing::decode_known::<HandshakeReply>(&buf).unwrap()
        };
        let (h, reply) = tokio::join!(host, ctl);
        assert!(h.is_err());
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
    }

    #[tokio::test]
    async fn approval_is_left_to_the_session() {
        let gate = TestGate::new(Access::Approval, None);
        let (c, h) = run(&gate, &Identity::generate(), None, None).await;
        assert!(c.is_ok());
        assert_eq!(h.unwrap().admission, Admission::NeedsApproval);
    }

    /// 中间人：换掉被控端的公钥、篡改握手消息，都连不上
    #[tokio::test]
    async fn tampering_is_detected() {
        let gate = TestGate::new(Access::Open, None);
        let me = Identity::generate();
        let ((mut cr, mut cw), (mut mr, mut mw)) = pipes();
        let ((mut hr, mut hw), (mut nr, mut nw)) = pipes();
        let intro = test_intro();
        let ctl = initiate(
            &mut cr,
            &mut cw,
            InitiatorParams {
                session_id: 1,
                code: 123_456,
                me: &me,
                host_key: None,
                pairing: None,
                intro: &intro,
            },
        );
        let host = respond(
            &mut nr,
            &mut nw,
            ResponderParams {
                session_id: 1,
                code: 123_456,
                gate: &gate,
            },
        );
        // 把控制端的第一条消息里的 Noise 消息改一个字节，被控端的回答原样转发，然后断开
        let relay = async move {
            let mut buf = Vec::new();
            read_frame(&mut mr, 1 << 16, &mut buf).await.unwrap();
            let mut init: HandshakeInit = framing::decode_known(&buf).unwrap();
            let last = init.noise.len() - 1;
            init.noise[last] ^= 1;
            write_frame(&mut hw, &framing::to_vec(&init)).await.unwrap();
            if read_frame(&mut hr, 1 << 16, &mut buf).await.unwrap() {
                write_frame(&mut mw, &buf).await.unwrap();
            }
            // 控制端失败之后不会再发什么；断开，被控端随之失败
            drop((mr, mw, hr, hw));
        };
        let (c, h, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(ctl, host, relay)
        })
        .await
        .unwrap();
        assert!(c.is_err() && h.is_err());
    }

    #[test]
    fn frames_fail_when_dropped_reordered_or_moved() {
        let a = Identity::generate();
        let b = Identity::generate();
        // 直接造一对 Secured（不经过网络）
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
        let ctl = Secured::new(Role::Controller, &mut i).unwrap();
        let host = Secured::new(Role::Host, &mut r).unwrap();

        let frames: Vec<Vec<u8>> = {
            let mut s = ctl.sealer(SessionLane::Control);
            (0..3u8)
                .map(|k| {
                    let mut f = framing::encode(&airhand_proto::session::ControllerMsg::Ack {
                        seq: k as u32,
                    });
                    s.seal_frame(&mut f).unwrap();
                    f[4..].to_vec()
                })
                .collect()
        };
        // 按顺序：都解得开
        let mut o = host.opener(SessionLane::Control);
        for f in &frames {
            o.open(&mut f.clone()).unwrap();
        }
        // 丢了一帧
        let mut o = host.opener(SessionLane::Control);
        o.open(&mut frames[0].clone()).unwrap();
        assert!(o.open(&mut frames[2].clone()).is_err());
        // 换到别的通道
        let mut o = host.opener(SessionLane::Video);
        assert!(o.open(&mut frames[0].clone()).is_err());
        // 方向反了（被控端自己发的不能当成收到的）
        let mut s = host.sealer(SessionLane::Control);
        let back = s.seal(b"x").unwrap();
        let mut o = host.opener(SessionLane::Control);
        assert!(o.open(&mut back.clone()).is_err());

        // 数据报：乱序可以，重放、篡改不行
        let mut ds = host.datagram_sealer();
        let mut dopen = ctl.datagram_opener();
        let d: Vec<Vec<u8>> = (0..4u8).map(|k| ds.seal(7, &[k; 10]).unwrap()).collect();
        let body = |d: &Vec<u8>| d[airhand_proto::datagram::HEADER..].to_vec();
        assert_eq!(dopen.open(&body(&d[1])).unwrap(), [1; 10]);
        assert_eq!(dopen.open(&body(&d[0])).unwrap(), [0; 10]);
        assert!(dopen.open(&body(&d[1])).is_none());
        let mut bad = body(&d[3]);
        bad[12] ^= 1;
        assert!(dopen.open(&bad).is_none());
        assert_eq!(dopen.open(&body(&d[3])).unwrap(), [3; 10]);
        assert_eq!(dopen.open(&body(&d[2])).unwrap(), [2; 10]);
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
