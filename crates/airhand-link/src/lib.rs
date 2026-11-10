//! Airhand 客户端一侧的链路，桌面客户端（`client/src-tauri`）、网页控制台（`web/`）和 Android（`android/native`）共用：
//!
//! * [`net`]：QUIC 连接、证书固定、版本握手、断线重连；
//! * [`transport`]：会话的四条流（控制、画面、声音、大块）；
//! * [`secure`]：端到端加密：设备的身份密钥、控制通道上的握手、每一帧的加解密；
//! * [`controller`]：控制端服务与会话。与界面无关：界面的命令是 [`controller::Command`]，
//!   画面和事件交给 [`controller::SessionUi`]；
//! * [`host`]：被控端与服务器的连接和会话的四条流（采集、编码、注入输入由各平台自己做）；
//! * [`flow`]：被控端的在途帧控制和自适应码率；
//! * [`Problem`]：显示给用户的问题，以类型而不是文字的形式传给界面。

pub mod controller;
pub mod flow;
pub mod host;
pub mod net;
mod problem;
pub mod secure;
pub mod tasks;
pub mod transport;

pub use net::{LinkConfig, ServerState};
pub use problem::Problem;
