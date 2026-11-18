#![allow(clippy::unwrap_used)]

use super::*;

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn rotates_and_bounds_total_size() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scenery.log");
    // 1 KiB segments, 3 backups => at most 4 files, ~4 KiB.
    let mut w = RotatingFile::new(path.clone(), 1024, 3).unwrap();
    let line = [b'x'; 190];
    for _ in 0..200 {
        w.write_all(&line).unwrap();
        w.write_all(b"\n").unwrap();
    }
    w.flush().unwrap();
    drop(w);

    assert_eq!(
        names(tmp.path()),
        [
            "scenery.log",
            "scenery.log.1",
            "scenery.log.2",
            "scenery.log.3"
        ]
    );
    let total: u64 = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= 4 * 1024, "total {total}");
}

#[test]
fn newest_backup_is_dot_one_and_reopen_appends() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scenery.log");
    let mut w = RotatingFile::new(path.clone(), 10, 2).unwrap();
    w.write_all(b"aaaaaaaa").unwrap();
    w.write_all(b"bbbbbbbb").unwrap(); // rolls: aaaa -> .1
    drop(w);
    assert_eq!(
        std::fs::read(tmp.path().join("scenery.log.1")).unwrap(),
        b"aaaaaaaa"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"bbbbbbbb");

    let mut w = RotatingFile::new(path.clone(), 100, 2).unwrap();
    w.write_all(b"c").unwrap();
    drop(w);
    assert_eq!(std::fs::read(&path).unwrap(), b"bbbbbbbbc");
}
