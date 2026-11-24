//! End-to-end terminal tests: libghostty-vt parsing, a real pty in both directions,
//! and key encoding into pty bytes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;
use sash::term::pty::Pty;
use sash::term::{FLAG_BOLD, Progress, ProgressState, Vt};

fn wait_for<F: Fn(&Vt) -> bool>(vt: &mut Vt, pty: &mut Pty, pred: F) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let bytes = pty.drain();
        if !bytes.is_empty() {
            vt.write(&bytes);
        }
        let replies = vt.take_replies();
        if !replies.is_empty() {
            pty.write(&replies);
        }
        vt.refresh(true);
        if pred(vt) {
            return true;
        }
        if pty.eof() {
            return pred(vt);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn row_text(vt: &Vt, y: u16) -> String {
    let grid = vt.grid();
    (0..grid.cols)
        .map(|x| grid.cell(x, y).map(|c| c.text.clone()).unwrap_or_default())
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn vt_parses_text_and_styles() {
    let mut vt = Vt::new(40, 5, 8, 16);
    vt.write(b"hello \x1b[1mbold\x1b[0m world\r\nsecond");
    vt.refresh(true);
    assert_eq!(row_text(&vt, 0), "hello bold world");
    let b = vt.grid().cell(6, 0).unwrap();
    assert!(b.flags & FLAG_BOLD != 0, "bold flag expected, got {}", b.flags);
    assert_eq!(row_text(&vt, 1), "second");
}

#[test]
fn vt_reports_cursor_position() {
    let mut vt = Vt::new(20, 5, 8, 16);
    vt.write(b"ab\x1b[3;4H");
    vt.refresh(true);
    assert_eq!(vt.grid().cursor, Some((3, 2)));
}

#[test]
fn pty_output_reaches_the_grid() {
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args(["-c", r"printf 'pty says \033[32mhi\033[0m\r\n'"]);
    let mut pty = Pty::spawn(cmd, 40, 6, 8, 16, "test".into(), Arc::new(|| {})).unwrap();
    let mut vt = Vt::new(40, 6, 8, 16);
    assert!(wait_for(&mut vt, &mut pty, |vt| row_text(vt, 0).contains("pty says hi")), "row: {:?}", row_text(&vt, 0));
}

#[test]
fn pty_input_reaches_the_program() {
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args(["-c", "IFS= read -r line; printf 'got:%s\\r\\n' \"$line\""]);
    let mut pty = Pty::spawn(cmd, 40, 6, 8, 16, "test".into(), Arc::new(|| {})).unwrap();
    let mut vt = Vt::new(40, 6, 8, 16);
    // The child waits on stdin until this write, so no sleep race.
    pty.write(b"hello from sash\r");
    let ok = wait_for(&mut vt, &mut pty, |vt| {
        (0..vt.grid().rows).any(|y| row_text(vt, y).contains("got:hello from sash"))
    });
    assert!(ok, "screen did not echo the typed line");
}

#[test]
fn keys_encode_to_pty_bytes() {
    use rune::{Key, KeyEvent, Mods};
    let mut vt = Vt::new(20, 5, 8, 16);

    let e = KeyEvent { key: Key::Text("a".into()), mods: Mods::default() };
    assert_eq!(vt.encode_key(&e), b"a");

    let e = KeyEvent { key: Key::Text("c".into()), mods: Mods { ctrl: true, ..Default::default() } };
    assert_eq!(vt.encode_key(&e), b"\x03");

    let e = KeyEvent { key: Key::Enter, mods: Mods::default() };
    assert_eq!(vt.encode_key(&e), b"\r");

    let e = KeyEvent { key: Key::Up, mods: Mods::default() };
    assert_eq!(vt.encode_key(&e), b"\x1b[A");

    let e = KeyEvent { key: Key::Tab, mods: Mods::default() };
    assert_eq!(vt.encode_key(&e), b"\t");
}

#[test]
fn the_first_refresh_draws_a_fresh_terminal() {
    // The constructor no longer snapshots at provisional metrics, so the first
    // refresh has to rebuild the whole grid or a restored tab would start blank.
    let mut vt = Vt::new(20, 4, 8, 16);
    vt.write(b"first frame");
    assert!(vt.refresh(false), "the first refresh must rebuild the grid");
    assert_eq!(row_text(&vt, 0), "first frame");
    assert!(
        !vt.refresh(false),
        "a second refresh with no output has nothing to do"
    );
}

#[test]
fn key_encoding_reuses_the_event_text_buffer() {
    use rune::{Key, KeyEvent, Mods};
    let mut vt = Vt::new(20, 5, 8, 16);
    let typed = |s: &str| KeyEvent {
        key: Key::Text(s.into()),
        mods: Mods::default(),
    };
    assert_eq!(vt.encode_key(&typed("a")), b"a");
    // A non-text key clears the buffer the event points into.
    assert_eq!(
        vt.encode_key(&KeyEvent {
            key: Key::Enter,
            mods: Mods::default()
        }),
        b"\r"
    );
    assert_eq!(vt.encode_key(&typed("b")), b"b");
    // A multi-byte character must survive the reused buffer intact.
    assert_eq!(vt.encode_key(&typed("é")), "é".as_bytes());
}

#[test]
fn resize_keeps_the_screen() {
    let mut vt = Vt::new(40, 10, 8, 16);
    vt.write(b"hello\r\nworld");
    vt.refresh(true);
    assert_eq!(row_text(&vt, 0), "hello");
    vt.resize(41, 12, 9, 17);
    vt.refresh(false);
    assert_eq!(row_text(&vt, 0), "hello", "after a resize");
    assert_eq!(row_text(&vt, 1), "world", "after a resize");
}

#[test]
fn resize_same_grid_new_pixels_keeps_the_screen() {
    let mut vt = Vt::new(40, 10, 8, 16);
    vt.write(b"keep me");
    vt.refresh(true);
    vt.resize(40, 10, 10, 20);
    vt.refresh(false);
    assert_eq!(row_text(&vt, 0), "keep me");
}

#[test]
fn scrollback_keeps_history_and_scrolls_both_ways() {
    let mut vt = Vt::new(20, 5, 8, 16);
    vt.set_scrollback(Some(500));
    for i in 0..40 {
        vt.write(format!("line {i:02}\r\n").as_bytes());
    }
    vt.refresh(true);
    let bottom = vt.scrollbar().expect("scrollbar geometry");
    assert!(bottom.total > bottom.len, "history should be scrollable: {bottom:?}");
    assert!(vt.viewport_active());

    vt.scroll(-10); // negative is up, libghostty-vt's sign
    let scrolled = vt.scrollbar().unwrap();
    assert!(!vt.viewport_active(), "scrolling up should leave the live viewport");
    assert!(scrolled.offset < bottom.offset, "offset should move toward history: {scrolled:?} vs {bottom:?}");

    vt.scroll_to_bottom();
    assert!(vt.viewport_active());
    let back = vt.scrollbar().unwrap();
    assert_eq!(back.offset, bottom.offset);
}

#[test]
fn a_foreground_child_is_detected() {
    let waker: sash::term::pty::Waker = Arc::new(|| {});
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", "set -m; sleep 30; true"]);
    let pty = Pty::spawn(command, 40, 12, 8, 16, "test".into(), waker).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    eprintln!(
        "shell={:?} fg={:?} busy={}",
        pty.pid(),
        pty.foreground_pgid(),
        pty.has_running_command()
    );
    assert!(pty.has_running_command());
}

#[test]
fn pwd_arrives_from_osc7() {
    let mut vt = Vt::new(40, 5, 8, 16);
    vt.write(b"\x1b]7;file://localhost/Users/me/My%20Code\x07");
    assert_eq!(vt.pwd(), Some(&PathBuf::from("/Users/me/My Code")));
}

#[test]
fn progress_and_desktop_notifications_come_from_osc() {
    let mut vt = Vt::new(40, 5, 8, 16);
    vt.write(b"\x1b]9;4;1;42\x07");
    assert_eq!(
        vt.progress(),
        Some(Progress { state: ProgressState::Determinate, percent: Some(42) })
    );
    vt.write(b"\x1b]9;4;2;80\x07");
    assert_eq!(
        vt.progress(),
        Some(Progress { state: ProgressState::Error, percent: Some(80) })
    );
    vt.write(b"\x1b]9;4;0\x07");
    assert_eq!(vt.progress(), None);

    vt.write(b"\x1b]9;Build done\x07");
    let notes = vt.take_notifications();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "Build done");
}

#[test]
fn osc8_hyperlinks_attach_to_cells() {
    let mut vt = Vt::new(40, 4, 8, 16);
    vt.write(b"\x1b]8;;https://example.com/a\x07link\x1b]8;;\x07 plain\r\n");
    vt.refresh(true);
    assert_eq!(
        vt.hyperlink_at(1, 0).as_deref(),
        Some("https://example.com/a")
    );
    assert_eq!(vt.hyperlink_at(5, 0), None);
}

#[test]
fn scrollback_search_finds_matches() {
    let mut vt = Vt::new(24, 4, 8, 16);
    for i in 0..30 {
        vt.write(format!("line {i} needle\r\n").as_bytes());
    }
    vt.refresh(true);
    assert!(vt.search_start("needle"));
    for _ in 0..10_000 {
        if vt.search_poll() {
            break;
        }
    }
    let (total, index) = vt.search_counts();
    assert_eq!(total, 30);
    assert!(index.is_none());
    assert!(vt.search_select(true));
    let (total, index) = vt.search_counts();
    assert_eq!(total, 30);
    assert!(index.is_some());
    assert!(!vt.search_highlight().is_empty());
    vt.search_clear();
    assert_eq!(vt.search_counts().0, 0);
}

#[test]
fn osc133_reports_command_end_and_exit() {
    let mut vt = Vt::new(40, 6, 8, 16);
    vt.write(b"\x1b]133;A\x07prompt$ \x1b]133;B\x07make\r\n");
    vt.write(b"\x1b]133;C;make test\x07output\r\n\x1b]133;D;7\x07");
    let end = vt.take_command_end().expect("command end");
    assert_eq!(end.exit, Some(7));
    assert!(vt.take_command_end().is_none());
}

#[test]
fn prompt_marks_scroll_by_command() {
    let mut vt = Vt::new(30, 5, 8, 16);
    for i in 0..40 {
        if i % 5 == 0 {
            vt.write(b"\x1b]133;A\x07prompt$ ");
        }
        vt.write(format!("line {i}\r\n").as_bytes());
    }
    vt.refresh(true);
    let bottom = vt.scrollbar().expect("scrollbar").offset;
    vt.scroll_prompts(-1);
    let after = vt.scrollbar().expect("scrollbar").offset;
    assert!(after < bottom, "{after} should be above {bottom}");
    vt.scroll_to_bottom();
    assert_eq!(vt.scrollbar().expect("scrollbar").offset, bottom);
}

#[test]
fn a_bell_is_reported_once() {
    let mut vt = Vt::new(20, 5, 8, 16);
    vt.write(b"\x07");
    assert!(vt.take_bell());
    assert!(!vt.take_bell());
}

#[test]
fn pty_output_wakes_the_app() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use sash::term::pty::Waker;

    // The reader thread must wake the UI thread itself; otherwise shell output
    // is only drained on the next keystroke or idle tick, which made typed
    // commands and Ctrl+C feel seconds late.
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let waker: Waker = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args(["-c", "printf 'wake me\\r\\n'"]);
    let pty = Pty::spawn(cmd, 40, 6, 8, 16, "test".into(), waker).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && hits.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(hits.load(Ordering::SeqCst) > 0, "the pty reader never woke the app");
    drop(pty);
}
