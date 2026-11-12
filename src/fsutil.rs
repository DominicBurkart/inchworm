use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

fn too_large(max_bytes: u64) -> io::Error {
    io::Error::other(format!("is larger than {max_bytes} bytes"))
}

/// Reads `reader` to the end as UTF-8 text, reading at most `max_bytes + 1`
/// bytes so an endless source cannot exhaust memory.
///
/// ```
/// use inchworm::fsutil::read_bounded;
///
/// assert_eq!(read_bounded(&b"abc"[..], 3).unwrap(), "abc");
/// assert!(read_bounded(&b"abcd"[..], 3).is_err());
/// ```
pub fn read_bounded<R: Read>(reader: R, max_bytes: u64) -> io::Result<String> {
    let mut text = String::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_string(&mut text)?;
    if text.len() as u64 > max_bytes {
        return Err(too_large(max_bytes));
    }
    Ok(text)
}

#[cfg(unix)]
fn open_for_read(path: &Path, follow_symlinks: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut flags = libc::O_NONBLOCK;
    if !follow_symlinks {
        flags |= libc::O_NOFOLLOW;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|err| {
            if !follow_symlinks && err.raw_os_error() == Some(libc::ELOOP) {
                io::Error::other("is a symlink, which is not followed")
            } else {
                err
            }
        })
}

#[cfg(not(unix))]
fn open_for_read(path: &Path, follow_symlinks: bool) -> io::Result<File> {
    if !follow_symlinks && std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::other("is a symlink, which is not followed"));
    }
    File::open(path)
}

fn read_checked(path: &Path, max_bytes: u64, follow_symlinks: bool) -> io::Result<String> {
    let file = open_for_read(path, follow_symlinks)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("is not a regular file"));
    }
    read_bounded(file, max_bytes)
}

/// Reads a regular file of at most `max_bytes` as UTF-8 text.
///
/// The file is opened first (without following a final symlink and without
/// blocking) and the type is checked on the open handle, so swapping the path
/// for a symlink or a FIFO between a check and the open cannot redirect or hang
/// the reader. At most `max_bytes + 1` bytes are ever read.
///
/// ```
/// use inchworm::fsutil::read_regular_text;
///
/// let dir = tempfile::tempdir().unwrap();
/// let file = dir.path().join("a.txt");
/// std::fs::write(&file, "hi").unwrap();
/// assert_eq!(read_regular_text(&file, 16).unwrap(), "hi");
/// assert!(read_regular_text(&file, 1).is_err());
/// assert!(read_regular_text(dir.path(), 16).is_err());
/// ```
pub fn read_regular_text(path: &Path, max_bytes: u64) -> io::Result<String> {
    read_checked(path, max_bytes, false)
}

/// Like [`read_regular_text`] but follows symlinks, for files the user owns
/// (for example a config managed by home-manager or stow). The target must
/// still be a regular file of at most `max_bytes`.
///
/// ```
/// use inchworm::fsutil::read_text_following_symlinks;
///
/// let dir = tempfile::tempdir().unwrap();
/// let file = dir.path().join("a.txt");
/// std::fs::write(&file, "hi").unwrap();
/// assert_eq!(read_text_following_symlinks(&file, 16).unwrap(), "hi");
/// assert!(read_text_following_symlinks(dir.path(), 16).is_err());
/// ```
pub fn read_text_following_symlinks(path: &Path, max_bytes: u64) -> io::Result<String> {
    read_checked(path, max_bytes, true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn within_ten_seconds<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the read hung")
    }

    fn make_fifo(path: &Path) {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", io::Error::last_os_error());
    }

    struct Overread {
        sent: u64,
        limit: u64,
    }

    impl Read for Overread {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.sent > self.limit {
                return Err(io::Error::other("the reader was read past its bound"));
            }
            buf.fill(b'a');
            self.sent += buf.len() as u64;
            Ok(buf.len())
        }
    }

    #[test]
    fn an_endless_reader_is_read_at_most_one_byte_past_the_limit() {
        let reader = Overread {
            sent: 0,
            limit: 64 * 1024,
        };
        let err = read_bounded(reader, 10).unwrap_err();
        assert!(err.to_string().contains("larger than 10 bytes"), "{err}");
    }

    #[test]
    fn the_bound_is_inclusive_for_any_reader() {
        assert_eq!(read_bounded(&b"12345"[..], 5).unwrap(), "12345");
        assert!(read_bounded(&b"123456"[..], 5).is_err());
        assert_eq!(read_bounded(&b""[..], 0).unwrap(), "");
        assert!(read_bounded(&b"x"[..], 0).is_err());
        assert!(read_bounded(&b"ok"[..], u64::MAX).is_ok());
    }

    #[test]
    fn symlinks_are_refused_even_when_they_point_at_a_small_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "hi").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = read_regular_text(&link, 16).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        make_fifo(&fifo);
        let err = within_ten_seconds(move || read_regular_text(&fifo, 16)).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_regular_text(dir.path(), 16).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn the_size_limit_is_inclusive_for_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "12345").unwrap();
        assert_eq!(read_regular_text(&file, 5).unwrap(), "12345");
        let err = read_regular_text(&file, 4).unwrap_err();
        assert!(err.to_string().contains("larger than 4 bytes"), "{err}");
    }

    #[test]
    fn missing_files_and_invalid_utf8_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = read_regular_text(&dir.path().join("nope"), 16).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        let bad = dir.path().join("bad");
        std::fs::write(&bad, [0xff, 0xfe]).unwrap();
        assert!(read_regular_text(&bad, 16).is_err());
    }

    #[test]
    fn following_symlinks_reads_the_target_and_still_checks_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.toml");
        std::fs::write(&target, "hi").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_text_following_symlinks(&link, 16).unwrap(), "hi");
        assert!(read_text_following_symlinks(&link, 1).is_err());

        let to_dir = dir.path().join("to-dir");
        std::os::unix::fs::symlink(dir.path(), &to_dir).unwrap();
        let err = read_text_following_symlinks(&to_dir, 16).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");

        let fifo = dir.path().join("pipe");
        make_fifo(&fifo);
        let to_fifo = dir.path().join("to-fifo");
        std::os::unix::fs::symlink(&fifo, &to_fifo).unwrap();
        let err =
            within_ten_seconds(move || read_text_following_symlinks(&to_fifo, 16)).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");

        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("gone"), &dangling).unwrap();
        let err = read_text_following_symlinks(&dangling, 16).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_path_swapped_between_a_regular_file_and_a_fifo_never_hangs_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("regular");
        std::fs::write(&regular, "hi").unwrap();
        let fifo = dir.path().join("fifo");
        make_fifo(&fifo);
        let target: PathBuf = dir.path().join("target");
        std::fs::hard_link(&regular, &target).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let stop = Arc::clone(&stop);
            let (regular, fifo, target) = (regular.clone(), fifo.clone(), target.clone());
            let scratch = dir.path().join("scratch");
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    for source in [&regular, &fifo] {
                        let _ = std::fs::remove_file(&scratch);
                        if std::fs::hard_link(source, &scratch).is_ok() {
                            let _ = std::fs::rename(&scratch, &target);
                        }
                    }
                }
            })
        };
        let outcome = within_ten_seconds(move || {
            let mut results = (0usize, 0usize);
            for _ in 0..2000 {
                match read_regular_text(&target, 16) {
                    Ok(text) => {
                        assert_eq!(text, "hi");
                        results.0 += 1;
                    }
                    Err(_) => results.1 += 1,
                }
            }
            (results, target)
        });
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert_eq!(outcome.0.0 + outcome.0.1, 2000);
    }
}
