//! Filesystem access that cannot be redirected: no symlink is followed below a starting
//! directory, only regular files are read, every read is bounded, and long reads stop when
//! told to.
//!
//! Everything here is blocking and runs on tokio's blocking pool. A path is walked one
//! component at a time with `openat(O_NOFOLLOW)`, so a symlink anywhere below the start
//! (not only in the last component, which is all `O_NOFOLLOW` alone covers) stops the walk.
//! Files are opened `O_NONBLOCK`, so a FIFO or a device opens without waiting for a writer,
//! and `fstat` on the opened descriptor then refuses it: the check and the read are about
//! the same file.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, mkdirat, openat, statat};
use rustix::io::Errno;

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Why a read stopped.
#[derive(Debug)]
pub enum ReadError {
    /// Not there.
    Missing,
    /// A symlink (anywhere on the way), a directory, a FIFO, a device: not a regular file.
    NotAFile(&'static str),
    /// Longer than allowed.
    TooBig,
    /// `stop()` said so (cancelled, or past a deadline).
    Stopped,
    /// Anything else.
    Io(io::Error),
}

impl From<Errno> for ReadError {
    fn from(e: Errno) -> Self {
        match e {
            Errno::NOENT => Self::Missing,
            Errno::LOOP | Errno::NOTDIR | Errno::MLINK => Self::NotAFile("a symlink"),
            e => Self::Io(e.into()),
        }
    }
}

/// Opens a starting directory. The start itself may be reached through symlinks (it is
/// the operator's configured path); nothing below it may.
pub fn open_root(path: &Path) -> Result<OwnedFd, ReadError> {
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Opens directory `name` in `dir`, without following a symlink. With `create`, makes it
/// (mode 0700) first if missing.
pub fn open_dir(dir: impl AsFd, name: &[u8], create: bool) -> Result<OwnedFd, ReadError> {
    if create {
        match mkdirat(&dir, name, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(openat(&dir, name, DIR, Mode::empty())?)
}

/// Walks the directories of a relative, `/`-separated path from `root`, without following
/// any symlink, and returns the last directory and the final name.
pub fn walk<'p>(root: &OwnedFd, rel: &'p [u8]) -> Result<(OwnedFd, &'p [u8]), ReadError> {
    let mut parts = rel
        .split(|&b| b == b'/')
        .filter(|p| !p.is_empty())
        .peekable();
    let mut dir = root.try_clone().map_err(ReadError::Io)?;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return Ok((dir, part));
        }
        if part == b".." || part == b"." {
            return Err(ReadError::NotAFile("a path that leaves its directory"));
        }
        dir = openat(&dir, part, DIR, Mode::empty())?;
    }
    Err(ReadError::Missing)
}

/// What a name in a directory is, without following it.
pub fn kind(dir: impl AsFd, name: &[u8]) -> Result<FileType, ReadError> {
    Ok(FileType::from_raw_mode(
        statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode,
    ))
}

/// The regular file `name` in `dir`, read in chunks into `sink`, at most `cap` bytes.
/// `stop` is asked between chunks. Returns whether the file is executable.
pub fn read_file(
    dir: impl AsFd,
    name: &[u8],
    cap: u64,
    stop: &dyn Fn() -> bool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<bool, ReadError> {
    let fd = openat(dir, name, FILE, Mode::empty())?;
    let st = fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(ReadError::NotAFile("not a regular file"));
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        if stop() {
            return Err(ReadError::Stopped);
        }
        let n = match rustix::io::read(&fd, &mut buf) {
            Ok(n) => n,
            Err(Errno::INTR | Errno::AGAIN) => continue,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Ok(st.st_mode & 0o111 != 0);
        }
        total += n as u64;
        if total > cap {
            return Err(ReadError::TooBig);
        }
        sink(&buf[..n]);
    }
}
