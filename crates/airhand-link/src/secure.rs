//! 端到端加密（协议见 `airhand_proto::handshake`）：设备的身份密钥、握手、之后每一帧的加解密。
//!
//! * [`Identity`]：一台设备的长期 X25519 密钥（Noise 的静态密钥），由各端自己保存；
//! * [`initiate`] / [`Initiator::finish`]：控制端开始握手（第一条消息放进发起会话的请求里），之后在控制通道上做完
//!   （最后是控制端的确认）；[`respond`]：被控端回答（发出决定就返回，确认由会话的读任务解开）。
//!   两边都得到 [`Secured`]；
//! * [`Secured`]：握手一次性派生的通道密钥：每条通道每个方向一个 [`Sealer`] / [`Opener`]，声音数据报、
//!   控制数据报各一对。
//!
//! 加解密都是原地做的：发送的帧预留了标签和计数的位置（`airhand_proto::framing::encode_with_spare`、
//! `airhand_proto::media::frame`），收到的帧解密之后截掉尾巴，画面包照样原样交给界面，不多拷贝。

use std::future::Future;
use std::time::Duration;

use crate::wire::{read_frame, write_msg};
use airhand_proto::framing::{self, PREFIX};
use airhand_proto::handshake::{
    self, Confirm, ControllerIntro, HandshakeError, HandshakeFinish, HandshakeInit, HandshakeReply,
    HostVerdict, MAX_MSG, NONCE_LEN, PAKE_CONTROLLER, PAKE_HOST, Pattern, PublicKey, Refusal,
    SEAL_OVERHEAD,
};
use airhand_proto::minicbor::{self, Encode};
use airhand_proto::session::{QualitySettings, StreamView};
use airhand_proto::{Role, SESSION_VERSION, SessionLane, datagram};
use anyhow::{Context, Result, anyhow, bail};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use spake2::{Ed25519Group, Identity as PakeId, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

use crate::Problem;

/// 握手的每一步最多等这么久
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
        let len = datagram::HEADER + packet.len() + SEAL_OVERHEAD;
        anyhow::ensure!(len <= datagram::MAX, "datagram payload too large");
        let mut d = Vec::with_capacity(len);
        d.extend_from_slice(&datagram::header(session_id, kind));
        d.extend_from_slice(packet);
        self.seal_at(&mut d, datagram::HEADER)?;
        Ok(d)
    }

    /// 控制消息直接编码到将要发送的缓冲区。
    pub(crate) fn seal_control(
        &mut self,
        session_id: u64,
        msg: &impl Encode<()>,
    ) -> Result<Vec<u8>> {
        let mut d = Vec::with_capacity(64);
        d.extend_from_slice(&datagram::header(session_id, datagram::CONTROL));
        minicbor::encode(msg, &mut d).expect("writing to a Vec can't fail");
        anyhow::ensure!(
            d.len() + SEAL_OVERHEAD <= datagram::MAX,
            "datagram payload too large"
        );
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    #[error("a session frame is too short")]
    Short,
    #[error("a session frame was dropped, reordered or replayed")]
    Reordered,
    #[error("a session frame failed authentication")]
    Authentication,
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
        let len = self.open_in_place(buf)?.len();
        buf.truncate(len);
        Ok(())
    }

    pub(crate) fn open_in_place<'a>(&mut self, buf: &'a mut [u8]) -> Result<&'a [u8], OpenError> {
        let c = counter_of(buf).ok_or(OpenError::Short)?;
        if !self.is_fresh(c) {
            return Err(OpenError::Reordered);
        }
        let at = buf.len() - NONCE_LEN;
        let plain = self
            .key
            .open_in_place(nonce(c), Aad::empty(), &mut buf[..at])
            .map_err(|_| OpenError::Authentication)?;
        // 认证通过之后才记下：伪造的单元不能把计数推走
        match &mut self.order {
            Order::Next(next) => *next = c.saturating_add(1),
            Order::Window(w) => w.accept(c),
        }
        Ok(plain)
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
    /// 画面的每一路（第 `i` 项是第 `i` 路，一共 [`airhand_proto::MAX_STREAMS`] 路，见 `airhand_proto::session` 的"多屏"）
    pub video: Vec<LaneKeys>,
    pub bulk: LaneKeys,
    /// 声音数据报。乱序可以、重放不行
    pub audio: LaneKeys,
    /// 控制数据报（Ping、Pong、画面确认、探测和回声，`airhand_proto::datagram::CONTROL`）。乱序可以、重放不行：
    /// 直连通着时同一个数据报两条路各发一份，后到的那份就是重放
    pub datagrams: LaneKeys,
    /// 剪贴板里的文件：每次取用一把密钥（见 [`FileKeys`]）
    pub files: FileKeys,
}

/// 剪贴板里的文件的密钥（见 `airhand_proto::files`）：每次取用一把，从每个方向一个的根按取用编号派生，计数从 0 开始、
/// 必须一个接一个。取用编号由取的一方分配，一个会话里严格递增（给的一方检查）：同一把密钥不会加密两份内容
pub struct FileKeys {
    /// 本端发出去的（本端给的取用）
    send: [u8; 32],
    /// 对方发来的（本端取的）
    recv: [u8; 32],
}

impl FileKeys {
    fn key(root: &[u8; 32], fetch: u32) -> LessSafeKey {
        let mut out = [0u8; 32];
        derive(
            root,
            root,
            format!("airhand file fetch {fetch}").as_bytes(),
            &mut out,
        );
        aead_key(&out)
    }

    /// 本端给对方的取用 `fetch` 的加密端
    pub fn sealer(&self, fetch: u32) -> Sealer {
        Sealer {
            key: Self::key(&self.send, fetch),
            counter: 0,
        }
    }

    /// 本端向对方取的取用 `fetch` 的解密端（必须一个接一个）
    pub fn opener(&self, fetch: u32) -> Opener {
        Opener {
            key: Self::key(&self.recv, fetch),
            order: Order::Next(0),
        }
    }

    /// 测试用：两端的一对（`a` 给、`b` 取，或者反过来）
    #[cfg(test)]
    pub(crate) fn test_pair() -> (Self, Self) {
        let (x, y) = ([1u8; 32], [2u8; 32]);
        (Self { send: x, recv: y }, Self { send: y, recv: x })
    }
}

impl Drop for FileKeys {
    fn drop(&mut self) {
        self.send = [0; 32];
        self.recv = [0; 32];
    }
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

/// 每条通道、每种数据报的密钥用不同的标签派生：从别的通道（画面的别的路）搬过来的帧解不开
fn label(lane: SessionLane) -> Vec<u8> {
    match lane {
        SessionLane::Control => b"airhand control".to_vec(),
        SessionLane::Video(0) => b"airhand video".to_vec(),
        SessionLane::Video(stream) => format!("airhand video {stream}").into_bytes(),
        SessionLane::Bulk => b"airhand bulk".to_vec(),
        SessionLane::File => b"airhand files".to_vec(),
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
                video: (0..airhand_proto::MAX_STREAMS)
                    .map(|s| self.keys(&label(SessionLane::Video(s)), window()))
                    .collect(),
                bulk: self.keys(&label(SessionLane::Bulk), Order::Next(0)),
                audio: self.keys(b"airhand audio", window()),
                datagrams: self.keys(b"airhand control datagram", window()),
                files: FileKeys {
                    send: self.raw(true, &label(SessionLane::File)),
                    recv: self.raw(false, &label(SessionLane::File)),
                },
            },
            self.keys(&label(SessionLane::Control), Order::Next(0)),
        )
    }

    /// 一个方向、一个标签的原始密钥
    fn raw(&self, sending: bool, label: &[u8]) -> [u8; 32] {
        // 控制端发送 = 发起方 → 响应方
        let i2r = sending == (self.role == Role::Controller);
        let ikm = if i2r { &self.i2r } else { &self.r2i };
        let mut out = [0u8; 32];
        derive(&self.h, ikm, label, &mut out);
        out
    }

    fn key(&self, sending: bool, label: &[u8]) -> LessSafeKey {
        aead_key(&self.raw(sending, label))
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
    tokio::time::timeout(STEP_TIMEOUT, write_msg(w, msg))
        .await
        .context("the end-to-end handshake writer timed out")??;
    Ok(())
}

/// Noise 消息及其明文都不能超过外层握手帧的上限。
fn noise_buf() -> Vec<u8> {
    vec![0; MAX_MSG]
}

/// 控制端握手的结果
pub struct Initiated {
    pub secured: Secured,
    /// 控制通道上被控端 → 控制端的接收端（已经解开了被控端的决定，计数接着往下数）
    pub opener: Opener,
    /// 控制通道上控制端 → 被控端的发送端（已经发出了确认，计数接着往下数）
    pub sealer: Sealer,
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
    version: u16,
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
    msg.truncate(n);
    init.noise = msg;
    let init = framing::to_vec(&init);
    anyhow::ensure!(init.len() <= MAX_MSG, "the handshake message is too large");
    Ok((
        Initiator {
            hs,
            pattern,
            pake: pake.map(|(s, _)| s),
            intro,
            version: p.intro.version,
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
            version,
        } = self;
        let mut buf = Vec::new();
        read_step(recv, &mut buf).await?;
        let reply: HandshakeReply = framing::decode_known(&buf)?;
        if let Some(e) = reply.error {
            return Err(handshake_problem(e, pattern).into());
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
            let n = hs.write_message(&intro, &mut payload)?;
            payload.truncate(n);
            send_step(send, &HandshakeFinish { noise: payload }).await?;
            read_step(recv, &mut buf).await?;
            let reply: HandshakeReply = framing::decode_known(&buf)?;
            if let Some(e) = reply.error {
                return Err(handshake_problem(e, pattern).into());
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
        if verdict.version != version {
            return Err(Problem::PeerIncompatible {
                peer: verdict.version,
                ours: version,
            }
            .into());
        }
        match verdict.refused {
            None => {}
            Some(Refusal::PairingRequired) => return Err(Problem::PairingRequired.into()),
            Some(r) => bail!("the host refused the connection ({r:?})"),
        }
        // 确认（见 airhand_proto::handshake 的"确认"）：被控端解开它才算本机进来了，重放的握手消息走不到这一步
        let mut confirm = framing::encode_with_spare(&Confirm::default(), SEAL_OVERHEAD);
        sealer.seal_frame(&mut confirm)?;
        tokio::time::timeout(STEP_TIMEOUT, send.write_all(&confirm))
            .await
            .context("the handshake confirmation writer timed out")??;
        debug!(?pattern, version, safety = %secured.safety, "end-to-end handshake done");
        Ok(Initiated {
            secured,
            opener,
            sealer,
        })
    }
}

fn handshake_problem(e: HandshakeError, pattern: Pattern) -> Problem {
    match e {
        HandshakeError::UnknownKey if pattern == Pattern::Known => Problem::HostKeyChanged,
        HandshakeError::PairingFailed => Problem::PairingFailed,
        HandshakeError::PairingUnavailable => Problem::PairingUnavailable,
        e => Problem::failed(format!("end-to-end handshake failed: {e:?}")),
    }
}

/// 一个已占名额的配对尝试。实现必须把失败和在途尝试一起计入本机预算；
/// 丢弃（包括超时、取消、格式错误）算一次未成功尝试。换配对码后旧尝试不能提交。
pub trait PairingAttempt: Send {
    fn code(&self) -> &str;
    /// 在本机同一把锁下检查代际并记住控制端，成功才返回 true。
    fn complete(self: Box<Self>, key: PublicKey, name: &str) -> bool;
}

/// 被控端：自己是谁，认得哪些控制端，配对尝试的本机预算（见 [`crate::host::pairing`]，各平台自己保存）。
///
/// 没配对过的控制端都要输入配对码：没有"知道控制码就能连"（服务器知道所有控制码，那样它自己就能连上来），
/// 也没有"请本机用户同意"
pub trait Gate: Send + Sync + 'static {
    fn identity(&self) -> Identity;
    /// 原子地占一个配对名额；None 表示现在不接受配对（暂停了、预算已用完）。
    fn begin_pairing(&self) -> Option<Box<dyn PairingAttempt + '_>> {
        None
    }
    /// 这个控制端配对过
    fn is_known(&self, key: &PublicKey) -> bool;
}

/// 被控端为什么让这个控制端进来
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// 刚刚用配对码配对成功
    Paired,
    /// 配对过的
    Known,
}

/// 被控端握手的结果。控制端还没确认（见 `airhand_proto::handshake` 的"确认"）：`opener` 解开的第一帧必须是
/// [`Confirm`]，在那之前不能显示"谁在连"
pub struct Responded {
    pub secured: Secured,
    /// 控制通道上被控端 → 控制端的发送端（已经发出了决定，计数接着往下数）
    pub sealer: Sealer,
    /// 控制通道上控制端 → 被控端的接收端（下一帧是确认，计数 0）
    pub opener: Opener,
    pub intro: ControllerIntro,
    pub admission: Admission,
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
    if init.len() > MAX_MSG {
        fail(send, HandshakeError::Malformed).await;
        bail!("the controller's first handshake message is too large");
    }
    let init: HandshakeInit = match framing::decode_known(init) {
        Ok(i) => i,
        Err(e) => {
            fail(send, HandshakeError::Malformed).await;
            return Err(e.into());
        }
    };
    let pattern = init.pattern;
    if !pattern.is_known() {
        fail(send, HandshakeError::Unsupported).await;
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
    msg.truncate(m);
    reply.noise = Some(msg);

    let (n, stream) = if pattern == Pattern::Known {
        (n, None)
    } else {
        send_step(send, &reply).await?;
        reply = HandshakeReply::default();
        let mut r = recv.await?;
        let mut buf = Vec::new();
        read_step(&mut r, &mut buf).await?;
        let finish: HandshakeFinish = framing::decode_known(&buf)?;
        match hs.read_message(&finish.noise, &mut payload) {
            Ok(n) => (n, Some(r)),
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
    let intro: ControllerIntro = match framing::decode_known(&payload[..n]) {
        Ok(i) => i,
        Err(e) => {
            fail(send, HandshakeError::Malformed).await;
            return Err(e.into());
        }
    };
    let (secured, LaneKeys { opener, mut sealer }) = KeyMaterial::new(Role::Host, &mut hs)?.split();

    // 先校验版本，只有同一会话格式才可能被接纳。
    let compatible = intro.version == SESSION_VERSION;
    let admission = if !compatible {
        None
    } else if pattern == Pattern::Pairing {
        Some(Admission::Paired)
    } else if p.gate.is_known(&secured.peer) {
        Some(Admission::Known)
    } else {
        None
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
        version: SESSION_VERSION,
        refused,
    };
    reply.verdict = Some(sealer.seal(&framing::to_vec(&verdict))?);
    send_step(send, &reply).await?;
    if let Some(r) = refused {
        bail!("refused the controller: {r:?}");
    }
    Ok((
        Responded {
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

/// 本端的介绍（控制端）：名称、系统，开始时的画质、要不要声音、看哪几个显示器
pub fn controller_intro(
    name: &str,
    os: &str,
    settings: QualitySettings,
    audio: bool,
    view: Vec<StreamView>,
) -> ControllerIntro {
    ControllerIntro {
        version: SESSION_VERSION,
        name: name.to_string(),
        os: os.to_string(),
        settings: settings.sanitized(),
        audio,
        view,
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
        code: Mutex<Option<String>>,
        failures: AtomicU32,
        known: Mutex<HashSet<PublicKey>>,
    }

    impl TestGate {
        fn new(code: Option<&str>) -> Self {
            Self {
                me: Identity::generate(),
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
        controller_intro("ctl", "test", settings, true, vec![StreamView::display(1)])
    }

    #[tokio::test]
    async fn first_contact_then_known_key() {
        let gate = TestGate::new(None);
        let me = Identity::generate();
        // 被控端认得这个控制端（配对过），控制端还不知道被控端的公钥（例如删掉了设备）：XX
        gate.known.lock().unwrap().insert(*me.public());
        let (c, h) = run(&gate, &me, None, None).await;
        let (c, h) = (c.unwrap(), h.unwrap());
        assert_eq!(c.secured.peer, *gate.me.public());
        assert_eq!(h.secured.peer, *me.public());
        assert_eq!(c.secured.safety, h.secured.safety);
        assert_eq!(h.admission, Admission::Known);
        assert_eq!(h.intro.name, "ctl");
        // 开始时的画质、声音和看哪个显示器在介绍里
        assert_eq!((h.intro.settings.fps, h.intro.audio), (60, true));
        assert_eq!(h.intro.view, [StreamView::display(1)]);
        assert_eq!(h.intro.version, SESSION_VERSION);

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
        // 画面的每一路一把密钥：别的路上的帧解不开
        let (mut h_video, mut c_video) = (h.secured.video, c.secured.video);
        assert_eq!(h_video.len(), airhand_proto::MAX_STREAMS as usize);
        let mut frame = airhand_proto::media::frame(&[7; 24], &[b"hello"]);
        h_video[1].sealer.seal_frame(&mut frame).unwrap();
        let mut payload = frame[4..].to_vec();
        assert!(c_video[0].opener.open(&mut payload.clone()).is_err());
        c_video[1].opener.open(&mut payload).unwrap();
        assert_eq!(&payload[24..], b"hello");
        let mut frame = airhand_proto::media::frame(&[7; 24], &[b"first"]);
        h_video[0].sealer.seal_frame(&mut frame).unwrap();
        let mut payload = frame[4..].to_vec();
        c_video[0].opener.open(&mut payload).unwrap();
        assert_eq!(&payload[24..], b"first");
        // 控制通道：被控端的发送端接着决定往下数，控制端的发送端接着确认往下数
        let (mut h_send, mut c_recv) = (h.sealer, c.opener);
        let mut f = framing::encode(&airhand_proto::session::HostMsg::Bye(
            airhand_proto::session::ByeReason::Kicked,
        ));
        h_send.seal_frame(&mut f).unwrap();
        let mut p = f[4..].to_vec();
        c_recv.open(&mut p).unwrap();
        let (mut c_send, mut h_recv) = (c.sealer, h.opener);
        let mut f = framing::encode(&airhand_proto::session::ControllerMsg::RequestKeyframe {
            after: 1,
            stream: 0,
        });
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
        let gate = TestGate::new(Some("123456"));
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
        let gate = TestGate::new(Some("482913"));
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
    /// 被控端不会当成那个配对过的控制端进来了（不显示、不提醒本机用户）
    #[tokio::test]
    async fn a_replayed_first_message_cannot_confirm() {
        let gate = TestGate::new(None);
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
        crate::wire::write_frame(&mut fake_w, &[0u8; 40])
            .await
            .unwrap();
        read_frame(&mut hr, 1 << 16, &mut buf).await.unwrap();
        let mut opener = r.opener;
        assert!(opener.open(&mut buf).is_err());
    }

    #[tokio::test]
    async fn a_session_version_mismatch_cannot_admit_or_pair() {
        let gate = TestGate::new(Some("123456"));
        let me = Identity::generate();
        for version in [SESSION_VERSION - 1, SESSION_VERSION + 1] {
            let intro = ControllerIntro {
                version,
                ..test_intro()
            };
            let (initiator, init) = initiate(params(&me, None, Some("123456"), &intro)).unwrap();
            let ((mut cr, mut cw), (hr, mut hw)) = pipes();
            let (controller, host) = tokio::join!(
                initiator.finish(&mut cr, &mut cw),
                respond(&init, &mut hw, async { Ok(hr) }, responder(&gate))
            );
            assert_eq!(
                problem(controller),
                Problem::PeerIncompatible {
                    peer: SESSION_VERSION,
                    ours: version
                }
            );
            assert!(host.is_err());
            assert!(!gate.is_known(me.public()));
        }
    }

    #[tokio::test]
    async fn bad_handshake_inits_are_rejected() {
        let gate = TestGate::new(None);
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

    /// 中间人：篡改握手消息，连不上
    #[tokio::test]
    async fn tampering_is_detected() {
        let gate = TestGate::new(None);
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
        let control = |m: &KeyMaterial| m.keys(&label(SessionLane::Control), Order::Next(0));
        let frames: Vec<Vec<u8>> = {
            let mut s = control(&ctl).sealer;
            (0..3u8)
                .map(|k| {
                    let mut f = framing::encode_with_spare(
                        &airhand_proto::session::ControllerMsg::RequestKeyframe {
                            after: k as u32,
                            stream: 0,
                        },
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
        let mut o = host.keys(&label(SessionLane::Bulk), Order::Next(0)).opener;
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
        // 控制数据报和声音的密钥不一样
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
