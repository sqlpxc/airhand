//! Materialize one top-level item at a time. Listings are relative to the offered folder.
//! Only directories we created inside the private stage can become a child's parent.

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use airhand_link::files::RemoteOffer;
use airhand_proto::files::{CHUNK, FileEntry, FileError, FileKind, MAX_TREE, valid_path};
use anyhow::{Context, Result, ensure};

use super::staging::{Stage, available_name, private_dir};

fn remote_error(e: FileError) -> anyhow::Error {
    anyhow::Error::msg(match e {
        FileError::TooLarge => tr!(
            "超出中转服务器的文件大小上限。",
            "The files exceed the relay's size limit."
        ),
        FileError::Changed | FileError::NotFound => tr!(
            "源文件已改变或被移除，请重新复制。",
            "The source file changed or was removed. Copy it again."
        ),
        FileError::Closed | FileError::Expired => tr!(
            "文件清单已失效，请在连接中重新复制。",
            "The offer expired. Copy the files again while connected."
        ),
        FileError::Denied => tr!(
            "对方没有权限读取源文件。",
            "The other device cannot read the source file."
        ),
        _ => tr!(
            "读取远程文件失败（{e:?}）。",
            "Could not read the remote files ({e:?})."
        ),
    })
}

fn new_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub(super) async fn download(offer: &RemoteOffer, stage: &Stage) -> Result<(usize, usize)> {
    let mut buf = vec![0; CHUNK];
    save(
        offer.items(),
        stage,
        async |path| offer.list(path, true).await.map_err(remote_error),
        async |path, entry, mut file| {
            let mut source = offer.open(path, entry.size, entry.mtime);
            while source.position() < entry.size {
                let n = source.read(&mut buf).await.map_err(remote_error)?;
                ensure!(n != 0, "incomplete file");
                file.write_all(&buf[..n])?;
                tokio::task::yield_now().await;
            }
            file.flush()?;
            Ok(())
        },
    )
    .await
}

async fn save(
    items: &[FileEntry],
    stage: &Stage,
    list: impl AsyncFn(&[String]) -> Result<Vec<FileEntry>>,
    mut fill: impl AsyncFnMut(Vec<String>, &FileEntry, File) -> Result<()>,
) -> Result<(usize, usize)> {
    let mut saved = 0;
    let mut skipped = 0;
    for root in items {
        tokio::task::yield_now().await;
        ensure!(
            root.path.len() == 1 && valid_path(&root.path),
            "invalid offered filename"
        );
        match root.kind {
            FileKind::File => fill(root.path.clone(), root, new_file(&stage.item)?)
                .await
                .with_context(|| root.name().to_owned())?,
            FileKind::Dir => {
                private_dir(&stage.item)?;
                let entries = list(&root.path).await?;
                ensure!(entries.len() <= MAX_TREE, "directory listing too large");
                let mut dirs = HashMap::from([(Vec::new(), stage.item.clone())]);
                for entry in entries {
                    tokio::task::yield_now().await;
                    ensure!(valid_path(&entry.path), "invalid path in directory listing");
                    let (name, parents) = entry.path.split_last().expect("validated path");
                    let parent = dirs
                        .get(parents)
                        .context("missing directory in file listing")?;
                    match entry.kind {
                        FileKind::Dir => {
                            let path = available_name(parent, name, |p| {
                                private_dir(p).map(|_| PathBuf::from(p))
                            })?;
                            ensure!(
                                dirs.insert(entry.path, path).is_none(),
                                "duplicate directory in file listing"
                            );
                        }
                        FileKind::File => {
                            let file = available_name(parent, name, new_file)?;
                            let mut remote = root.path.clone();
                            remote.extend_from_slice(&entry.path);
                            fill(remote, &entry, file)
                                .await
                                .with_context(|| entry.name().to_owned())?;
                        }
                        _ => skipped += 1,
                    }
                }
            }
            _ => {
                skipped += 1;
                continue;
            }
        }
        stage
            .publish(root.name())
            .with_context(|| root.name().to_owned())?;
        saved += 1;
    }
    Ok((saved, skipped))
}
