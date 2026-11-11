//! 无界面被控端（开发调试用）：`cargo run --example headless_host`
use airhand_lib::{host::HostService, status};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info,quinn=warn")
        .init();
    let hub = status::new_hub();
    let _host = HostService::start(hub.clone());
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
