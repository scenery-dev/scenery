#![allow(clippy::unwrap_used)]
use super::*;
use camino::Utf8PathBuf;

fn tmp() -> (tempfile::TempDir, Utf8PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = Utf8PathBuf::from_path_buf(d.path().to_path_buf()).unwrap();
    (d, p)
}

fn tar_with<F: FnOnce(&mut tar::Builder<flate2::write::GzEncoder<fs::File>>)>(
    root: &Utf8Path,
    f: F,
) -> Utf8PathBuf {
    let path = root.join("a.cskill");
    let gz = flate2::write::GzEncoder::new(
        fs::File::create(&path).unwrap(),
        flate2::Compression::default(),
    );
    let mut b = tar::Builder::new(gz);
    f(&mut b);
    b.into_inner().unwrap().finish().unwrap();
    path
}

fn add_file<W: Write>(b: &mut tar::Builder<W>, name: &str, body: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_size(body.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    b.append_data(&mut h, name, body).unwrap();
}

fn add_symlink<W: Write>(b: &mut tar::Builder<W>, name: &str, target: &str) {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    h.set_mode(0o777);
    b.append_link(&mut h, name, target).unwrap();
}

#[test]
fn symlink_then_write_through_link_is_refused_and_nothing_escapes() {
    let (_d, root) = tmp();
    let victim = root.join("victim");
    fs::create_dir(&victim).unwrap();
    let ar = tar_with(&root, |b| {
        add_symlink(b, "x", victim.as_str());
        add_file(b, "x/authorized_keys", b"pwn");
    });
    let dest = root.join("out");
    let err = unpack_cskill_archive(&ar, &dest).unwrap_err();
    assert!(format!("{err:#}").contains("symlink"), "{err:#}");
    assert!(!victim.join("authorized_keys").exists());
}

#[test]
fn symlink_alone_and_other_special_entries_are_refused() {
    let (_d, root) = tmp();
    let ar = tar_with(&root, |b| {
        add_file(b, "SKILL.md", b"x");
        add_symlink(b, "link", "/etc/passwd");
    });
    assert!(unpack_cskill_archive(&ar, &root.join("o1")).is_err());
    let ar = tar_with(&root, |b| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Fifo);
        h.set_size(0);
        b.append_data(&mut h, "fifo", &b""[..]).unwrap();
    });
    assert!(unpack_cskill_archive(&ar, &root.join("o2")).is_err());
}

#[test]
fn regular_archive_with_wrapper_dir_unpacks_stripped() {
    let (_d, root) = tmp();
    let ar = tar_with(&root, |b| {
        add_file(b, "bundle/SKILL.md", b"hello");
        add_file(b, "bundle/prompts/a.txt", b"p");
    });
    let dest = root.join("out");
    unpack_cskill_archive(&ar, &dest).unwrap();
    assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "hello");
    assert_eq!(fs::read_to_string(dest.join("prompts/a.txt")).unwrap(), "p");
}

#[test]
fn traversal_paths_are_refused() {
    let (_d, root) = tmp();
    let path = root.join("evil.cskill");
    // The tar builder refuses ".." itself, so write the header by hand.
    let gz = flate2::write::GzEncoder::new(
        fs::File::create(&path).unwrap(),
        flate2::Compression::default(),
    );
    let mut b = tar::Builder::new(gz);
    let mut h = tar::Header::new_gnu();
    h.set_size(1);
    h.set_mode(0o644);
    {
        let name = b"../escape.txt";
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
    }
    h.set_cksum();
    b.append(&h, &b"x"[..]).unwrap();
    b.into_inner().unwrap().finish().unwrap();
    assert!(unpack_cskill_archive(&path, &root.join("out")).is_err());
    assert!(!root.join("escape.txt").exists());
}

#[test]
fn entry_count_overflow_is_refused() {
    let (_d, root) = tmp();
    let ar = tar_with(&root, |b| {
        for i in 0..=MAX_ENTRIES {
            add_file(b, &format!("f{i}.txt"), b"");
        }
    });
    let err = unpack_cskill_archive(&ar, &root.join("out")).unwrap_err();
    assert!(format!("{err:#}").contains("entries"), "{err:#}");
}

#[test]
fn tar_bomb_entry_over_cap_is_refused_by_bytes_written() {
    let (_d, root) = tmp();
    // 51 MiB of zeroes compresses to a few KiB.
    let ar = tar_with(&root, |b| {
        add_file(b, "SKILL.md", b"x");
        let big = vec![0u8; (MAX_ENTRY_BYTES + 1024) as usize];
        add_file(b, "big.bin", &big);
    });
    let err = unpack_cskill_archive(&ar, &root.join("out")).unwrap_err();
    assert!(format!("{err:#}").contains("exceeds"), "{err:#}");
}

#[test]
fn zip_symlink_entry_is_refused() {
    let (_d, root) = tmp();
    let path = root.join("z.cskill");
    {
        let f = fs::File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        z.add_symlink("link", "/etc/passwd", opts).unwrap();
        z.finish().unwrap();
    }
    let err = unpack_cskill_archive(&path, &root.join("out")).unwrap_err();
    assert!(format!("{err:#}").contains("symlink"), "{err:#}");
}

#[test]
fn zip_regular_unpacks() {
    let (_d, root) = tmp();
    let path = root.join("z.cskill");
    {
        let f = fs::File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("SKILL.md", opts).unwrap();
        z.write_all(b"hi").unwrap();
        z.finish().unwrap();
    }
    let dest = root.join("out");
    unpack_cskill_archive(&path, &dest).unwrap();
    assert_eq!(fs::read_to_string(dest.join("SKILL.md")).unwrap(), "hi");
}
