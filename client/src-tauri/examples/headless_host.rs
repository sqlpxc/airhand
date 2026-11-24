//! 无界面被控端（开发调试用）：`cargo run --example headless_host`
use airhand_lib::host::{HostCode, HostService};
use airhand_lib::status;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info,quinn=warn")
        .init();
    // 和正式的被控端一样：双显卡电脑上桌面复制要找对显卡，在用到 DXGI 之前
    #[cfg(windows)]
    airhand_lib::host::capture::ignore_gpu_preference();
    let hub = status::new_hub();
    let _host = HostService::start(hub.clone(), HostCode::default());
    let mut rx = hub.subscribe();
    loop {
        let st = rx.borrow_and_update().clone();
        println!(
            "server: {:?} code: {:?} pairing: {:?} controllers: {:?}",
            st.host.server, st.host.code, st.host.pairing, st.host.controllers
        );
        if rx.changed().await.is_err() {
            break;
        }
    }
}
