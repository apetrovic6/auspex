//! Review finding 3: the theme file was read with `read_to_string`, no type check and no
//! cap, after the termination signals were taken over, so a FIFO blocked auspex forever
//! and `/dev/zero` read until memory ran out, with Ctrl-C doing nothing. Now only a regular
//! file of at most `theme::MAX_BYTES` is read, and anything else falls back to ANSI with a
//! note. Each case runs under a time bound so a regression fails instead of hanging.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use auspex_tui::theme::{self, Theme};
use common::{tempdir, tools};

fn load_within(path: PathBuf) -> (Theme, Option<String>) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(theme::load(Some(&path)));
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("loading the theme returned: no blocking read")
}

#[test]
fn a_fifo_theme_falls_back_without_blocking() {
    let Some(t) = tools(&["mkfifo"]) else { return };
    let dir = tempdir();
    let fifo = dir.path().join("theme.toml");
    assert!(
        Command::new(t.get("mkfifo"))
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let (theme, note) = load_within(fifo);
    assert_eq!(theme, Theme::Ansi);
    assert!(note.unwrap().contains("not a regular file"));
}

#[test]
fn a_device_theme_falls_back_without_reading_it() {
    if !Path::new("/dev/zero").exists() {
        eprintln!("SKIPPED: no /dev/zero");
        return;
    }
    let (theme, note) = load_within(PathBuf::from("/dev/zero"));
    assert_eq!(theme, Theme::Ansi);
    assert!(note.unwrap().contains("not a regular file"));
}

#[test]
fn an_oversized_theme_falls_back() {
    let dir = tempdir();
    let big = dir.path().join("theme.toml");
    std::fs::write(&big, "#".repeat(theme::MAX_BYTES + 1)).unwrap();
    let (theme, note) = load_within(big);
    assert_eq!(theme, Theme::Ansi);
    assert!(note.unwrap().contains("larger than"));
}

#[test]
fn a_regular_theme_still_loads() {
    let dir = tempdir();
    let file = dir.path().join("theme.toml");
    let mut text = String::new();
    for i in 0..16 {
        let _ = writeln!(text, "base{i:02X} = \"#1020{i:02x}\"");
    }
    std::fs::write(&file, text).unwrap();
    let (theme, note) = load_within(file);
    assert!(matches!(theme, Theme::Base16(_)), "{note:?}");
    assert_eq!(note, None);
}
