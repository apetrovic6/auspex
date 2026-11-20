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

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat, fstat, mkdirat, openat, statat};
use rustix::io::Errno;

/// A file's identity and change time: device, inode, `ctime` (seconds and nanoseconds) and
/// size, from one `stat`. Any write, truncation, `chmod`, `utimes` or rename sets `ctime` to
/// the current time, and no unprivileged call sets it back, so two equal stamps of one name
/// mean nothing replaced or changed that file in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp([i128; 5]);

impl Stamp {
    /// The stamp of a `stat` result.
    #[must_use]
    pub fn of(st: &Stat) -> Self {
        Self([
            i128::from(st.st_dev),
            i128::from(st.st_ino),
            i128::from(st.st_ctime),
            i128::from(st.st_ctime_nsec),
            i128::from(st.st_size),
        ])
    }

    /// The stamp of an open file or directory.
    pub fn of_fd(fd: impl AsFd) -> Result<Self, ReadError> {
        Ok(Self::of(&fstat(fd)?))
    }

    /// The stamp as bytes, for a digest.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 80] {
        let mut b = [0u8; 80];
        for (chunk, v) in b.as_chunks_mut::<16>().0.iter_mut().zip(self.0) {
            chunk.copy_from_slice(&v.to_be_bytes());
        }
        b
    }
}

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// `O_PATH` exists on Linux only. Elsewhere (macOS, which the flake also builds for) a name-only
/// open does not exist, so the directory is opened `O_RDONLY`: it then needs read permission as
/// well as search, so it can refuse a directory Linux would accept, and never accepts one Linux
/// would refuse. Without this the crate did not compile on darwin at all (found by
/// `just cross-check`).
#[cfg(any(target_os = "linux", target_os = "android"))]
const PATH_FLAG: OFlags = OFlags::PATH;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const PATH_FLAG: OFlags = OFlags::RDONLY;

/// A directory opened only to name things in it (`O_PATH`): needs search permission on
/// the way to it, not read permission on it, as git itself does in `.git` (review of
/// `ca2cc3f`, F6). `statat` and `openat` relative to it work; listing it does not.
const PATH_DIR: OFlags = PATH_FLAG
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

/// Opens a starting directory with `O_PATH` (see [`PATH_DIR`]). Like [`open_root`], the
/// start itself may be reached through symlinks.
pub fn open_path_root(path: &Path) -> Result<OwnedFd, ReadError> {
    Ok(rustix::fs::open(
        path,
        PATH_FLAG | OFlags::DIRECTORY | OFlags::CLOEXEC,
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

/// What [`walk`] found: the last directory and the final name, or why it stopped and the
/// deepest directory it reached.
pub type Walked<'p> = Result<(OwnedFd, &'p [u8]), (ReadError, Option<OwnedFd>)>;

/// Walks the directories of a relative, `/`-separated path from `root`, without following
/// any symlink, and returns the last directory and the final name.
///
/// # Errors
///
/// Why the walk stopped, with the deepest directory it reached (`root` at worst).
pub fn walk<'p>(root: &OwnedFd, rel: &'p [u8]) -> Walked<'p> {
    walk_with(root, rel, DIR)
}

/// [`walk`] with every directory opened `O_PATH` (see [`PATH_DIR`]).
pub fn walk_path<'p>(root: &OwnedFd, rel: &'p [u8]) -> Walked<'p> {
    walk_with(root, rel, PATH_DIR)
}

fn walk_with<'p>(root: &OwnedFd, rel: &'p [u8], flags: OFlags) -> Walked<'p> {
    let mut parts = rel
        .split(|&b| b == b'/')
        .filter(|p| !p.is_empty())
        .peekable();
    let mut dir = root.try_clone().map_err(|e| (ReadError::Io(e), None))?;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return Ok((dir, part));
        }
        if part == b".." || part == b"." {
            return Err((
                ReadError::NotAFile("a path that leaves its directory"),
                Some(dir),
            ));
        }
        dir = match openat(&dir, part, flags, Mode::empty()) {
            Ok(d) => d,
            Err(e) => return Err((e.into(), Some(dir))),
        };
    }
    Err((ReadError::Missing, Some(dir)))
}

/// What a name in a directory is, without following it, and its [`Stamp`].
pub fn kind(dir: impl AsFd, name: &[u8]) -> Result<(FileType, Stamp), ReadError> {
    let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok((FileType::from_raw_mode(st.st_mode), Stamp::of(&st)))
}

/// What a regular file read by [`read_file`] was.
#[derive(Debug, Clone, Copy)]
pub struct FileRead {
    /// Whether the file is executable by its owner.
    pub exec: bool,
    /// Its [`Stamp`], from the descriptor that was read, taken before the first byte.
    pub stamp: Stamp,
}

/// The regular file `name` in `dir`, read in chunks into `sink`, at most `cap` bytes.
/// `stop` is asked between chunks.
pub fn read_file(
    dir: impl AsFd,
    name: &[u8],
    cap: u64,
    stop: &dyn Fn() -> bool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<FileRead, ReadError> {
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
            // The owner bit only, as git and nix use it (review finding N2a).
            return Ok(FileRead {
                exec: st.st_mode & 0o100 != 0,
                stamp: Stamp::of(&st),
            });
        }
        total += n as u64;
        if total > cap {
            return Err(ReadError::TooBig);
        }
        sink(&buf[..n]);
    }
}
