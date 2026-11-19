use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;

use airhand_proto::files::{FileEntry, FileKind};
use tempfile::tempdir;
use tokio::sync::oneshot;

use super::super::staging::{Stage, receive, recover};
use super::save;

fn entry(path: &[&str], kind: FileKind) -> FileEntry {
    FileEntry {
        path: path.iter().map(|s| (*s).into()).collect(),
        kind,
        size: 0,
        mtime: None,
        target: None,
    }
}

#[tokio::test]
async fn saves_a_tree_without_overwriting_or_following_links() {
    let config = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let out = dest.path();
    fs::create_dir(out.join("photos")).unwrap();
    fs::write(out.join("original"), b"keep").unwrap();
    symlink(out.join("original"), out.join("note.txt")).unwrap();
    let roots = [
        entry(&["photos"], FileKind::Dir),
        entry(&["note.txt"], FileKind::File),
    ];
    let tree = vec![
        entry(&["empty"], FileKind::Dir),
        entry(&["sub"], FileKind::Dir),
        entry(&["sub", "x:y"], FileKind::File),
        entry(&["sub", "x_y"], FileKind::File),
        entry(&["link"], FileKind::Link),
    ];
    let (_tx, rx) = oneshot::channel();
    let got = receive(config.path(), out, rx, async |stage| {
        save(
            &roots,
            stage,
            async |path| {
                assert_eq!(path, ["photos"]);
                Ok(tree.clone())
            },
            async |path, _, mut f| {
                f.write_all(path.join("/").as_bytes())?;
                Ok(())
            },
        )
        .await
    })
    .await
    .unwrap();
    assert_eq!(got, Some((2, 1)));
    assert!(out.join("photos (2)/empty").is_dir());
    assert_eq!(
        fs::read(out.join("photos (2)/sub/x_y")).unwrap(),
        b"photos/sub/x:y"
    );
    assert_eq!(
        fs::read(out.join("photos (2)/sub/x_y (2)")).unwrap(),
        b"photos/sub/x_y"
    );
    assert!(!out.join("photos (2)/link").exists());
    assert_eq!(fs::read(out.join("note (2).txt")).unwrap(), b"note.txt");
    assert_eq!(fs::read(out.join("note.txt")).unwrap(), b"keep");
    assert_eq!(out.read_dir().unwrap().count(), 5);
    assert_eq!(config.path().read_dir().unwrap().count(), 0);
}

#[tokio::test]
async fn invalid_listing_discards_current_tree_and_keeps_completed_items() {
    let config = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let roots = [
        entry(&["zero"], FileKind::File),
        entry(&["folder"], FileKind::Dir),
    ];
    let (_tx, rx) = oneshot::channel();
    let r = receive(config.path(), dest.path(), rx, async |stage| {
        save(
            &roots,
            stage,
            async |_| Ok(vec![entry(&["..", "outside"], FileKind::File)]),
            async |_, _, _| Ok(()),
        )
        .await
    })
    .await;
    assert!(r.is_err());
    assert!(dest.path().join("zero").is_file());
    assert_eq!(dest.path().read_dir().unwrap().count(), 1);
    assert_eq!(config.path().read_dir().unwrap().count(), 0);
}

#[tokio::test]
async fn cancelling_a_stalled_read_removes_only_the_partial() {
    let config = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let (tx, rx) = oneshot::channel();
    let (started, wait) = oneshot::channel();
    let transfer = receive(config.path(), dest.path(), rx, async |stage| {
        fs::write(&stage.item, b"done")?;
        stage.publish("finished")?;
        fs::write(&stage.item, b"partial")?;
        started.send(()).unwrap();
        std::future::pending::<anyhow::Result<()>>().await
    });
    let stop = async {
        wait.await.unwrap();
        assert_eq!(config.path().read_dir().unwrap().count(), 1);
        drop(tx);
    };
    let (result, ()) = tokio::join!(transfer, stop);
    assert_eq!(result.unwrap(), None);
    assert_eq!(fs::read(dest.path().join("finished")).unwrap(), b"done");
    assert_eq!(dest.path().read_dir().unwrap().count(), 1);
    assert_eq!(config.path().read_dir().unwrap().count(), 0);
}

#[test]
fn crash_recovery_uses_only_the_recorded_stage() {
    let config = tempdir().unwrap();
    let dest = tempdir().unwrap();
    fs::write(config.path().join("config.toml"), b"settings").unwrap();
    let other = dest
        .path()
        .join(".airhand-receive-00000000000000000000000000000000");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("keep"), b"keep").unwrap();
    {
        let stage = Stage::create(config.path(), dest.path()).unwrap();
        fs::create_dir(&stage.item).unwrap();
        symlink(&other, stage.item.join("outside")).unwrap();
        fs::write(stage.item.join("unfinished"), b"partial").unwrap();
        // Simulate a process killed before it could clean up. Only the recovery link survives.
    }
    recover(config.path()).unwrap();
    recover(config.path()).unwrap();
    assert_eq!(dest.path().read_dir().unwrap().count(), 1);
    assert_eq!(fs::read(other.join("keep")).unwrap(), b"keep");
    assert_eq!(config.path().read_dir().unwrap().count(), 1);
    assert_eq!(
        fs::read(config.path().join("config.toml")).unwrap(),
        b"settings"
    );
}

#[test]
fn cleanup_refuses_a_replaced_folder_or_unrelated_record() {
    let config = tempdir().unwrap();
    let dest = tempdir().unwrap();
    let other = tempdir().unwrap();
    let stage = Stage::create(config.path(), dest.path()).unwrap();
    let record = config
        .path()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let path = fs::read_link(&record).unwrap();
    fs::remove_dir(&path).unwrap();
    symlink(other.path(), &path).unwrap();
    let keep = other.path().join(stage.item.file_name().unwrap());
    fs::write(&keep, b"keep").unwrap();
    assert!(recover(config.path()).is_err());
    assert_eq!(fs::read(&keep).unwrap(), b"keep");
    fs::remove_file(&record).unwrap();
    symlink(other.path(), &record).unwrap();
    assert!(recover(config.path()).is_err());
    assert!(keep.exists());
}

#[test]
fn a_missing_volume_does_not_block_another_receive() {
    let config = tempdir().unwrap();
    let disks = tempdir().unwrap();
    let first = disks.path().join("first");
    let second = disks.path().join("second");
    let offline = disks.path().join("offline");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let stage = Stage::create(config.path(), &first).unwrap();
    fs::write(stage.item, b"partial").unwrap();
    fs::rename(&first, &offline).unwrap();
    assert!(recover(config.path()).is_err());
    let next = Stage::create(config.path(), &second).unwrap();
    fs::write(&next.item, b"done").unwrap();
    next.publish("finished").unwrap();
    next.clean().unwrap();
    fs::rename(offline, &first).unwrap();
    recover(config.path()).unwrap();
    assert_eq!(fs::read(second.join("finished")).unwrap(), b"done");
    assert_eq!(first.read_dir().unwrap().count(), 0);
    assert_eq!(config.path().read_dir().unwrap().count(), 0);
}
