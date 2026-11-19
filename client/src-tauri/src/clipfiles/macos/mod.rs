//! macOS receives clipboard offers from the menu bar. The general pasteboard is independent.

mod staging;
mod transfer;

use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use airhand_link::files::RemoteOffer;
use anyhow::Result;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSModalResponseOK, NSOpenPanel};
use objc2_foundation::NSString;
use tauri::AppHandle;
use tokio::sync::oneshot;

use crate::status::StatusHub;

#[derive(Default)]
struct Inbox {
    offer: Option<RemoteOffer>,
    choosing: bool,
    job: Option<Job>,
    stopped: bool,
}

struct Job {
    cancel: Option<oneshot::Sender<()>>,
    worker: JoinHandle<()>,
}

fn inbox() -> &'static Mutex<Inbox> {
    static INBOX: OnceLock<Mutex<Inbox>> = OnceLock::new();
    INBOX.get_or_init(Default::default)
}

static HUB: OnceLock<StatusHub> = OnceLock::new();

pub fn init(hub: &StatusHub) {
    let _ = HUB.set(hub.clone());
    if let Err(e) = recover() {
        tracing::warn!("cleaning up an interrupted file transfer: {e}");
    }
}

fn refresh() {
    if let Some(hub) = HUB.get() {
        hub.send_modify(|_| {});
    }
}

pub(super) fn apply(offer: RemoteOffer) {
    let mut state = inbox().lock().unwrap();
    if !state.stopped && offer.is_current() {
        state.offer = Some(offer.clone());
    }
    drop(state);
    offer.ready();
    refresh();
}

pub(super) fn withdraw(key: u64) {
    let mut state = inbox().lock().unwrap();
    if state.offer.as_ref().is_some_and(|o| o.key() == key) {
        state.offer = None;
        drop(state);
        refresh();
    }
}

impl Inbox {
    fn available(&self) -> Option<&RemoteOffer> {
        self.offer
            .as_ref()
            .filter(|o| !o.items().is_empty() && o.is_current())
    }
}

pub fn menu_item() -> (&'static str, String, bool) {
    let state = inbox().lock().unwrap();
    if let Some(job) = &state.job {
        (
            "cancel_receive",
            tr!("取消接收文件", "Cancel file transfer"),
            job.cancel.is_some(),
        )
    } else {
        (
            "receive_files",
            tr!("接收文件到…", "Receive files to…"),
            !state.stopped && !state.choosing && state.available().is_some(),
        )
    }
}

pub fn cancel() {
    if let Some(tx) = inbox()
        .lock()
        .unwrap()
        .job
        .as_mut()
        .and_then(|j| j.cancel.take())
    {
        let _ = tx.send(());
    }
    refresh();
}

/// Snapshot before showing the panel. A copy in another session while the panel is open
/// cannot switch which files the user is accepting.
pub fn choose(app: &AppHandle) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let offer = {
        let mut state = inbox().lock().unwrap();
        if state.choosing || state.job.is_some() || state.stopped {
            return;
        }
        let Some(offer) = state.available().cloned() else {
            return;
        };
        state.choosing = true;
        offer
    };
    refresh();
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setCanCreateDirectories(true);
    panel.setAllowsMultipleSelection(false);
    let count = offer.items().len();
    let first = offer.items()[0].name();
    panel.setMessage(Some(&NSString::from_str(&tr!(
        "选择接收“{first}”等 {count} 项的文件夹。",
        "Choose a folder for {count} item(s), starting with “{first}”."
    ))));
    panel.setPrompt(Some(&NSString::from_str(&tr!("接收", "Receive"))));
    crate::tray::front_panel(mtm, &panel);
    let dest = (panel.runModal() == NSModalResponseOK)
        .then(|| panel.URL().and_then(|u| u.to_file_path()))
        .flatten();
    let mut state = inbox().lock().unwrap();
    state.choosing = false;
    let result = match dest {
        Some(dest) if !state.stopped && state.available() == Some(&offer) => {
            Job::start(app.clone(), offer, dest)
                .map(|job| state.job = Some(job))
                .map_err(|e| e.to_string())
        }
        Some(_) if !state.stopped => Err(tr!(
            "文件清单已改变，请重新选择“接收文件到…”。",
            "The file offer changed. Choose Receive files to… again."
        )),
        _ => Ok(()),
    };
    drop(state);
    refresh();
    if let Err(e) = result {
        message_box(&e);
    }
}

fn message_box(message: &str) {
    if crate::EXITING.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str(&tr!("好", "OK")));
    crate::tray::front_panel(mtm, &alert.window());
    alert.runModal();
}

impl Job {
    fn start(app: AppHandle, offer: RemoteOffer, dest: PathBuf) -> io::Result<Self> {
        let (tx, rx) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("airhand-receive".into())
            .spawn(move || {
                let result = offer.runtime().block_on(async {
                    staging::receive(&staging::config_dir()?, &dest, rx, async |stage| {
                        transfer::download(&offer, stage).await
                    })
                    .await
                });
                Self::finished(&app, result);
            })?;
        Ok(Self {
            cancel: Some(tx),
            worker,
        })
    }

    fn finished(app: &AppHandle, result: Result<Option<(usize, usize)>>) {
        let mut state = inbox().lock().unwrap();
        state.job = None;
        let stopped = state.stopped;
        drop(state);
        if stopped {
            return;
        }
        refresh();
        let message = match result {
            Ok(Some((n, 0))) => tr!("已接收 {n} 项。", "Received {n} item(s)."),
            Ok(Some((n, skipped))) => tr!(
                "已接收 {n} 项，跳过了 {skipped} 个链接或不支持的项目。",
                "Received {n} item(s). Skipped {skipped} links or unsupported entries."
            ),
            Ok(None) => return,
            Err(e) => tr!(
                "接收文件失败。\n{e:#}",
                "Could not receive the files.\n{e:#}"
            ),
        };
        let _ = app.run_on_main_thread(move || message_box(&message));
    }
}

fn recover() -> io::Result<()> {
    staging::recover(&staging::config_dir()?)
}

/// The worker must finish before uninstall removes its recovery record or resets permissions.
pub fn shutdown() -> io::Result<()> {
    let job = {
        let mut state = inbox().lock().unwrap();
        state.stopped = true;
        state.offer = None;
        state.job.take()
    };
    if let Some(job) = job {
        drop(job.cancel);
        let _ = job.worker.join();
    }
    recover()
}
