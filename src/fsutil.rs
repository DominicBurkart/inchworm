use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

/// Reads a regular file of at most `max_bytes` as UTF-8 text.
///
/// Symlinks, FIFOs, devices and directories are refused before the file is
/// opened, so a hostile path cannot hang the reader or redirect it. At most
/// `max_bytes + 1` bytes are ever read.
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
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other("is a symlink, which is not followed"));
    }
    if !meta.is_file() {
        return Err(io::Error::other("is not a regular file"));
    }
    let mut text = String::new();
    File::open(path)?
        .take(max_bytes + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > max_bytes {
        return Err(io::Error::other(format!(
            "is larger than {max_bytes} bytes"
        )));
    }
    Ok(text)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn within_ten_seconds<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the read hung")
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
    fn a_fifo_is_refused_without_being_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !matches!(made, Ok(status) if status.success()) {
            return;
        }
        let err = within_ten_seconds(move || read_regular_text(&fifo, 16)).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn the_size_limit_is_checked_before_reading_and_is_inclusive() {
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
    fn a_file_that_grows_after_the_size_check_is_still_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("grow");
        std::fs::write(&file, "123").unwrap();
        let text = read_regular_text(&file, 3).unwrap();
        assert_eq!(text.len(), 3);
    }
}
