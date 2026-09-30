//! The directory as the console browses it: the tree a deploy would ship, and one file at a time.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use cloud_api::{FileContent, FileEntry, FileKind, FileTree};

use crate::error::{WorkdirError, io};
use crate::walk::{self, PRIVATE};

/// How much of a file decides whether it is text: a NUL or invalid UTF-8 in it means binary.
pub const SNIFF_BYTES: usize = 8 * 1024;

/// Every entry of the shared walk (the one [`crate::pack`] archives from), directories included,
/// in path order, at most `max_entries` of them.
///
/// The one difference from the archive: the root `deploy.json` is listed (people want to see
/// it) although [`crate::pack`] leaves it out. Sizes are the file's length; `0` for directories
/// and symlinks, which the archive stores without contents.
pub fn list_files(root: &Path, max_entries: usize) -> Result<FileTree, WorkdirError> {
    let root = root.canonicalize().map_err(io(root))?;
    let mut files = Vec::new();
    for item in walk::walk(&root) {
        if files.len() == max_entries {
            return Ok(FileTree { files, truncated: true });
        }
        let entry = item?;
        let size = match entry.kind {
            FileKind::File => entry.dent.metadata().map_err(|e| walk::walk_error(&root, &e))?.len(),
            FileKind::Dir | FileKind::Symlink => 0,
        };
        files.push(FileEntry { path: entry.rel, kind: entry.kind, size });
    }
    Ok(FileTree { files, truncated: false })
}

/// The file at `rel` (`/`-separated, relative to `root`), its text cut to at most `max_bytes`
/// bytes on a character boundary.
///
/// Refused: absolute paths, `.`/`..`/empty segments, NUL and `\` ([`WorkdirError::BadPath`]);
/// anything inside `.git` or `.aho`, asked for directly or reached through a symlink
/// ([`WorkdirError::Private`]); anything a symlink resolves to outside `root`
/// ([`WorkdirError::Escapes`]); anything but a regular file ([`WorkdirError::NotAFile`]).
/// Ignored files are served: the tree does not list them, but they are the user's own.
///
/// A file whose first [`SNIFF_BYTES`] hold a NUL or invalid UTF-8 is binary: `text: None`.
pub fn read_file(root: &Path, rel: &str, max_bytes: u64) -> Result<FileContent, WorkdirError> {
    check_rel(rel)?;
    let root = root.canonicalize().map_err(io(root))?;
    let joined = root.join(rel);
    let real = match joined.canonicalize() {
        Ok(p) => p,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
            return Err(WorkdirError::NotFound(rel.to_owned()));
        }
        Err(e) => return Err(WorkdirError::Io { path: joined, source: e }),
    };
    let inner = real.strip_prefix(&root).map_err(|_| WorkdirError::Escapes(rel.to_owned()))?;
    if inner.components().any(|c| c.as_os_str().to_str().is_some_and(|n| PRIVATE.contains(&n))) {
        return Err(WorkdirError::Private(rel.to_owned()));
    }
    // Checked before opening: opening a FIFO for reading would block until a writer shows up.
    if !std::fs::metadata(&real).map_err(io(&real))?.is_file() {
        return Err(WorkdirError::NotAFile(rel.to_owned()));
    }
    let file = File::open(&real).map_err(io(&real))?;
    let meta = file.metadata().map_err(io(&real))?;
    if !meta.is_file() {
        return Err(WorkdirError::NotAFile(rel.to_owned()));
    }
    let want = max_bytes.max(SNIFF_BYTES as u64);
    let mut buf = Vec::with_capacity(usize::try_from(meta.len().min(want)).unwrap_or(0).saturating_add(1));
    file.take(want.saturating_add(1)).read_to_end(&mut buf).map_err(io(&real))?;
    // Bytes exist beyond what was read.
    let more = buf.len() as u64 > want;
    buf.truncate(usize::try_from(want).unwrap_or(usize::MAX));

    let sniff = &buf[..buf.len().min(SNIFF_BYTES)];
    let sniff_cut = more || buf.len() > sniff.len();
    let binary = sniff.contains(&0)
        || match std::str::from_utf8(sniff) {
            Ok(_) => false,
            // An incomplete character is only fine where the sniff window cut it.
            Err(e) => e.error_len().is_some() || !sniff_cut,
        };
    if binary {
        return Ok(FileContent { path: rel.to_owned(), size: meta.len(), text: None, truncated: false });
    }

    let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let mut truncated = more || buf.len() > cap;
    buf.truncate(cap);
    if truncated {
        drop_partial_char(&mut buf);
    }
    let mut text = String::from_utf8(buf).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    // Replacement characters are wider than the bytes they replace.
    if text.len() > cap {
        let mut end = cap;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    Ok(FileContent { path: rel.to_owned(), size: meta.len(), text: Some(text), truncated })
}

fn check_rel(rel: &str) -> Result<(), WorkdirError> {
    let bad = || WorkdirError::BadPath(rel.to_owned());
    if rel.is_empty() || rel.contains(['\0', '\\']) || rel.starts_with('/') {
        return Err(bad());
    }
    for segment in rel.split('/') {
        if matches!(segment, "" | "." | "..") {
            return Err(bad());
        }
        if PRIVATE.contains(&segment) {
            return Err(WorkdirError::Private(rel.to_owned()));
        }
    }
    // `C:`-style prefixes on Windows; on Unix every segment above is a normal component.
    if !Path::new(rel).components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(bad());
    }
    Ok(())
}

/// Drops a UTF-8 sequence the cut left incomplete at the end of `bytes`.
fn drop_partial_char(bytes: &mut Vec<u8>) {
    let len = bytes.len();
    // A sequence is at most four bytes, so its lead byte is at most three back from the end.
    for back in 1..=len.min(3) {
        let b = bytes[len - back];
        if b & 0xC0 == 0x80 {
            continue;
        }
        let width = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        if width > back {
            bytes.truncate(len - back);
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tests::tree;

    fn kinds(t: &FileTree) -> Vec<(&str, FileKind)> {
        t.files.iter().map(|f| (f.path.as_str(), f.kind)).collect()
    }

    #[test]
    fn the_tree_is_the_archive_plus_deploy_json_and_directories() {
        let dir = tree();
        let t = list_files(dir.path(), 5000).unwrap();
        assert!(!t.truncated);
        assert_eq!(
            kinds(&t),
            [
                (".env", FileKind::File),
                (".env.example", FileKind::File),
                (".gitignore", FileKind::File),
                ("deploy.json", FileKind::File),
                ("src", FileKind::Dir),
                ("src/main.rs", FileKind::File),
                ("sub", FileKind::Dir),
                ("sub/.gitignore", FileKind::File),
                ("sub/secret.txt", FileKind::File),
            ]
        );
        let packed = crate::pack(dir.path()).unwrap().files;
        let listed: Vec<&str> =
            t.files.iter().filter(|f| f.kind != FileKind::Dir && f.path != "deploy.json").map(|f| f.path.as_str()).collect();
        assert_eq!(listed, packed, "exactly the archived files, in archive order");
        let main = t.files.iter().find(|f| f.path == "src/main.rs").unwrap();
        assert_eq!(main.size, "fn main() {}".len() as u64);
        assert_eq!(t.files.iter().find(|f| f.path == "src").unwrap().size, 0);
    }

    #[test]
    fn paths_are_ordered_component_by_component() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["src.rs", "src/main.rs", "a-b", "a/z"] {
            fs::create_dir_all(dir.path().join(f).parent().unwrap()).unwrap();
            fs::write(dir.path().join(f), "").unwrap();
        }
        let t = list_files(dir.path(), 100).unwrap();
        let paths: Vec<&str> = t.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a", "a/z", "a-b", "src", "src/main.rs", "src.rs"]);
    }

    #[test]
    fn a_capped_tree_says_it_is_truncated() {
        let dir = tree();
        let all = list_files(dir.path(), 5000).unwrap();
        let n = all.files.len();
        let capped = list_files(dir.path(), 3).unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.files, all.files[..3]);
        let exact = list_files(dir.path(), n).unwrap();
        assert!(!exact.truncated, "a cap equal to the entry count loses nothing");
        assert_eq!(exact.files, all.files);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_listed_as_links() {
        let dir = tree();
        std::os::unix::fs::symlink("/etc", dir.path().join("etc")).unwrap();
        let t = list_files(dir.path(), 5000).unwrap();
        let link = t.files.iter().find(|f| f.path == "etc").unwrap();
        assert_eq!((link.kind, link.size), (FileKind::Symlink, 0));
        assert!(!t.files.iter().any(|f| f.path.starts_with("etc/")), "never followed");
    }

    #[test]
    fn reads_text_and_the_manifest() {
        let dir = tree();
        let c = read_file(dir.path(), "src/main.rs", 1 << 20).unwrap();
        assert_eq!(c, FileContent { path: "src/main.rs".into(), size: 12, text: Some("fn main() {}".into()), truncated: false });
        assert_eq!(read_file(dir.path(), "deploy.json", 1 << 20).unwrap().text.as_deref(), Some("{}"));
    }

    #[test]
    fn malformed_paths_are_refused() {
        let dir = tree();
        for bad in
            ["", "/etc/passwd", "../x", "src/../../x", "src/..", "./src/main.rs", "src//main.rs", "src/", "a\\b", "src/main.rs\0", "."]
        {
            assert!(matches!(read_file(dir.path(), bad, 1024), Err(WorkdirError::BadPath(_))), "{bad:?}");
        }
    }

    #[test]
    fn git_and_aho_are_private_at_any_depth() {
        let dir = tree();
        fs::create_dir_all(dir.path().join("sub/.git")).unwrap();
        fs::write(dir.path().join("sub/.git/config"), "x").unwrap();
        for private in [".git/HEAD", ".aho/sessions/1.jsonl", ".aho", "sub/.git/config"] {
            assert!(matches!(read_file(dir.path(), private, 1024), Err(WorkdirError::Private(_))), "{private}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_resolve_but_never_out_of_the_root_or_into_private_dirs() {
        use std::os::unix::fs::symlink;
        let dir = tree();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("host-secret"), "no").unwrap();
        symlink(outside.path().join("host-secret"), dir.path().join("out")).unwrap();
        symlink(outside.path(), dir.path().join("outdir")).unwrap();
        symlink("../..", dir.path().join("sub/up")).unwrap();
        symlink(".aho/sessions/1.jsonl", dir.path().join("log")).unwrap();
        symlink("src/main.rs", dir.path().join("main")).unwrap();
        assert!(matches!(read_file(dir.path(), "out", 1024), Err(WorkdirError::Escapes(_))));
        assert!(matches!(read_file(dir.path(), "outdir/host-secret", 1024), Err(WorkdirError::Escapes(_))));
        let outside_name = outside.path().file_name().unwrap().to_str().unwrap();
        assert!(matches!(read_file(dir.path(), &format!("sub/up/{outside_name}/host-secret"), 1024), Err(WorkdirError::Escapes(_))));
        // Out and back in lands inside the root, which is fine.
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(read_file(dir.path(), &format!("sub/up/{name}/src/main.rs"), 1024).unwrap().text.as_deref(), Some("fn main() {}"));
        assert!(matches!(read_file(dir.path(), "log", 1024), Err(WorkdirError::Private(_))));
        let inside = read_file(dir.path(), "main", 1024).unwrap();
        assert_eq!((inside.path.as_str(), inside.text.as_deref()), ("main", Some("fn main() {}")));
    }

    #[test]
    fn missing_files_and_directories_are_told_apart() {
        let dir = tree();
        assert!(matches!(read_file(dir.path(), "nope.txt", 1024), Err(WorkdirError::NotFound(_))));
        assert!(matches!(read_file(dir.path(), "src/main.rs/x", 1024), Err(WorkdirError::NotFound(_))));
        assert!(matches!(read_file(dir.path(), "src", 1024), Err(WorkdirError::NotAFile(_))));
    }

    #[test]
    fn binary_is_a_nul_or_invalid_utf8_in_the_first_8_kib() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("nul.bin"), b"ab\0cd").unwrap();
        fs::write(p.join("latin1.txt"), b"caf\xe9 au lait").unwrap();
        fs::write(p.join("cut.txt"), b"ends mid-char \xe2\x82").unwrap();
        let mut late = vec![b'a'; SNIFF_BYTES];
        late.extend_from_slice(b"\0\xff");
        fs::write(p.join("late.txt"), &late).unwrap();
        // A multi-byte character straddling the sniff window is still text.
        let mut straddle = vec![b'a'; SNIFF_BYTES - 1];
        straddle.extend_from_slice("€ ok".as_bytes());
        fs::write(p.join("straddle.txt"), &straddle).unwrap();

        for f in ["nul.bin", "latin1.txt", "cut.txt"] {
            let c = read_file(p, f, 1 << 20).unwrap();
            assert_eq!((c.text, c.truncated), (None, false), "{f}");
        }
        let c = read_file(p, "nul.bin", 1 << 20).unwrap();
        assert_eq!(c.size, 5);
        let late = read_file(p, "late.txt", 1 << 20).unwrap().text.unwrap();
        assert!(late.starts_with("aaa") && late.ends_with("\0\u{fffd}"), "past the window it is text, repaired");
        assert!(read_file(p, "straddle.txt", 1 << 20).unwrap().text.unwrap().ends_with("€ ok"));
    }

    #[test]
    fn text_is_cut_at_max_bytes_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("t.txt"), "aé€😀z").unwrap(); // 1 + 2 + 3 + 4 + 1 bytes
        let at = |n| read_file(dir.path(), "t.txt", n).unwrap();
        for (max, want) in [(1, "a"), (2, "a"), (3, "aé"), (5, "aé"), (6, "aé€"), (9, "aé€"), (10, "aé€😀")] {
            let c = at(max);
            assert_eq!((c.text.as_deref(), c.truncated, c.size), (Some(want), true, 11), "max {max}");
        }
        let whole = at(11);
        assert_eq!((whole.text.as_deref(), whole.truncated), (Some("aé€😀z"), false));
        assert!(!at(1 << 20).truncated);

        let big = "x".repeat(3 * SNIFF_BYTES);
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        let c = read_file(dir.path(), "big.txt", 1 << 20).unwrap();
        assert_eq!((c.text.as_deref(), c.truncated), (Some(big.as_str()), false), "the sniff window does not cap the text");
        let c = read_file(dir.path(), "big.txt", 100).unwrap();
        assert_eq!((c.text.map(|t| t.len()), c.truncated), (Some(100), true));
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("mkfifo").arg(dir.path().join("pipe")).status().unwrap();
        assert!(status.success());
        assert!(matches!(read_file(dir.path(), "pipe", 1024), Err(WorkdirError::NotAFile(_))));
    }
}
