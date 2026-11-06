use crate::cell::{attr, Cell, Color, Rgb, Theme};
use crate::input::{Key, Mods, MouseAction, MouseButton, MouseEvent};
use crate::terminal::{CursorShape, MouseMode, Point, SelectionMode, Terminal};

fn term(cols: usize, rows: usize) -> Terminal {
    Terminal::new(cols, rows, 1000, Theme::default())
}

fn feed(t: &mut Terminal, s: &str) {
    t.feed(s.as_bytes());
}

fn row_text(t: &Terminal, y: usize) -> String {
    let mut buf = Vec::new();
    let cells = t.visible_row(y, &mut buf);
    let mut s = String::new();
    for c in cells.iter().filter(|c| !c.is_spacer()) {
        t.clusters.push_str(c, &mut s);
    }
    s.trim_end().to_string()
}

fn screen(t: &Terminal) -> Vec<String> {
    (0..t.rows()).map(|y| row_text(t, y)).collect()
}

fn cell(t: &Terminal, x: usize, y: usize) -> Cell {
    t.screen()[y].cells[x]
}

fn cur(t: &Terminal) -> (usize, usize) {
    (t.cur.x, t.cur.y)
}

/// Everything observable, for comparing two terminals.
type Snapshot = (Vec<Vec<Cell>>, Vec<bool>, (usize, usize, bool), String, Vec<u8>, Vec<String>);

fn snapshot(t: &Terminal) -> Snapshot {
    let rows = t.screen().iter().map(|r| r.cells.clone()).collect();
    let wraps = t.screen().iter().map(|r| r.wrapped).collect();
    let clusters = t.clusters.list.iter().map(|s| s.to_string()).collect();
    let mut sb = Vec::new();
    for l in &t.sb.lines {
        sb.extend_from_slice(l);
    }
    (rows, wraps, (t.cur.x, t.cur.y, t.cur.pending), t.title.clone(), sb, clusters)
}

const BAD: &[u8] = b"\xff\xc3\x28 bad \xe2\x82 x\x1bP1$qm\x1b\\\x1b_apc\x1b\\done";

fn sample() -> Vec<u8> {
    let (a, b) = SAMPLE.split_at(SAMPLE.find("\r\n1\r\n").unwrap());
    [a.as_bytes(), BAD, b.as_bytes()].concat()
}

const SAMPLE: &str = concat!(
    "\x1b]0;tïtle\x07hello \x1b[1;31mwörld\x1b[0m\r\n",
    "\x1b[38;2;10;20;30m日本語\x1b[48:5:200mx\x1b[m e\u{301} 👍🏽 ❤\u{fe0f}\r\n",
    "\x1b[2;5Hmid\x1b[K\x1b[?25l\x1b[?2026h\x1b[3A\x1b[2Kredraw\x1b[?2026l\x1b[?25h",
    "\x1b]8;id=1;https://x.y\x1b\\link\x1b]8;;\x1b\\ \x1b(0lqk\x1b(B\x1b[5b",
    "\x1b[10;20r\x1b[15H\x1b[3L\x1b[2M\x1b[r\x1b[999;999H\x1b[6n",
    "\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\n11\r\n12\r\n",
);

#[test]
fn split_input_equals_whole() {
    let bytes = &sample()[..];
    let mut whole = term(30, 8);
    whole.feed(bytes);
    let mut bytewise = term(30, 8);
    for b in bytes {
        bytewise.feed(std::slice::from_ref(b));
    }
    assert_eq!(snapshot(&whole), snapshot(&bytewise));
    assert_eq!(whole.take_responses(), bytewise.take_responses());
    let mut seed = 12345u32;
    for _ in 0..50 {
        let mut t = term(30, 8);
        let mut i = 0;
        while i < bytes.len() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let n = 1 + (seed >> 16) as usize % 7;
            t.feed(&bytes[i..(i + n).min(bytes.len())]);
            i += n;
        }
        assert_eq!(snapshot(&whole), snapshot(&t));
    }
}

#[test]
fn garbage_does_not_panic() {
    let mut seed = 7u32;
    let mut data = Vec::new();
    for _ in 0..200_000 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let b = (seed >> 24) as u8;
        data.push(if b.is_multiple_of(5) { 0x1b } else if b.is_multiple_of(7) { b'[' } else { b });
    }
    for cols in [1, 2, 3, 80] {
        let mut t = term(cols, 5);
        t.feed(&data);
        t.resize(cols + 3, 2);
        t.feed(&data[..5000]);
        t.resize(1, 1);
        t.feed(&data[..5000]);
        let _ = t.take_damage();
        let _ = t.selected_text();
    }
}

#[test]
fn utf8_invalid_and_split() {
    let mut t = term(20, 2);
    t.feed(b"a\xffb\xe2\x82c\xc3");
    t.feed(b"\xa9\xf0\x9f");
    t.feed(b"\x98\x80!");
    assert_eq!(row_text(&t, 0), "a\u{fffd}b\u{fffd}cé😀!");
    let mut t = term(20, 2);
    t.feed(b"\xe0\x80\x80x\xed\xa0\x80y");
    assert_eq!(row_text(&t, 0), "\u{fffd}x\u{fffd}y");
}

#[test]
fn sgr_combinations() {
    let mut t = term(40, 2);
    feed(&mut t, "\x1b[1;2;3;4;7;8;9mA\x1b[22;23;24;27;28;29mB");
    assert_eq!(cell(&t, 0, 0).attrs, attr::BOLD | attr::DIM | attr::ITALIC | attr::UNDERLINE | attr::REVERSE | attr::INVISIBLE | attr::STRIKE);
    assert_eq!(cell(&t, 1, 0).attrs, 0);
    feed(&mut t, "\x1b[31;42mC\x1b[91;102mD\x1b[39;49mE");
    assert_eq!((cell(&t, 2, 0).fg, cell(&t, 2, 0).bg), (Color::indexed(1), Color::indexed(2)));
    assert_eq!((cell(&t, 3, 0).fg, cell(&t, 3, 0).bg), (Color::indexed(9), Color::indexed(10)));
    assert_eq!((cell(&t, 4, 0).fg, cell(&t, 4, 0).bg), (Color::DEFAULT, Color::DEFAULT));
    feed(&mut t, "\x1b[38;5;123;48;2;1;2;3;1mF");
    let f = cell(&t, 5, 0);
    assert_eq!((f.fg, f.bg, f.attrs), (Color::indexed(123), Color::rgb(1, 2, 3), attr::BOLD));
    feed(&mut t, "\x1b[0;38:2::4:5:6;48:5:7mG\x1b[38:2:7:8:9mH\x1b[4:3mI\x1b[4:0mJ\x1b[58:2::1:2:3mK");
    assert_eq!((cell(&t, 6, 0).fg, cell(&t, 6, 0).bg), (Color::rgb(4, 5, 6), Color::indexed(7)));
    assert_eq!(cell(&t, 7, 0).fg, Color::rgb(7, 8, 9));
    assert_eq!(cell(&t, 8, 0).attrs, attr::UNDERLINE);
    assert_eq!(cell(&t, 9, 0).attrs, 0);
    assert_eq!(cell(&t, 10, 0).fg, Color::rgb(7, 8, 9));
    feed(&mut t, "\x1b[mL\x1b[5;6;25;53;21mM");
    assert_eq!(cell(&t, 11, 0), Cell { ch: 'L' as u32, ..Cell::default() });
    assert_eq!(cell(&t, 12, 0).attrs, attr::UNDERLINE);
    let (fg, bg) = t.cell_colors(&Cell { attrs: attr::REVERSE, ..Cell::default() });
    assert_eq!((fg, bg), (Theme::default().bg, Theme::default().fg));
}

#[test]
fn wide_chars() {
    let mut t = term(5, 4);
    feed(&mut t, "abcd日");
    assert_eq!(row_text(&t, 0), "abcd");
    assert!(t.screen()[0].wrapped);
    assert_eq!(row_text(&t, 1), "日");
    assert!(cell(&t, 0, 1).is_wide() && cell(&t, 1, 1).is_spacer());
    assert_eq!(cur(&t), (2, 1));
    feed(&mut t, "\x1b[2;2Hx");
    assert_eq!(row_text(&t, 1), " x");
    assert!(!cell(&t, 0, 1).is_wide());
    feed(&mut t, "\x1b[3;1H日本語");
    assert_eq!(row_text(&t, 2), "日本");
    assert_eq!(row_text(&t, 3), "語");
    assert_eq!(cur(&t), (2, 3));
    feed(&mut t, "\x1b[4;1Habc日");
    assert_eq!(cur(&t), (4, 3));
    assert!(t.cur.pending);
    feed(&mut t, "\x1b[3;1H\x1b[@");
    assert_eq!(row_text(&t, 2), " 日本");
    feed(&mut t, "\x1b[3;3H\x1b[P");
    assert_eq!(row_text(&t, 2), "  本");
    feed(&mut t, "\x1b[3;1H\x1b[2@");
    assert_eq!(row_text(&t, 2), "");
}

#[test]
fn combining_and_emoji() {
    let mut t = term(10, 2);
    feed(&mut t, "e\u{301}x👍\u{1f3fd}👨\u{200d}👩\u{200d}👧!");
    let mut b = [0; 4];
    assert_eq!(t.cell_str(&cell(&t, 0, 0), &mut b), "e\u{301}");
    assert_eq!(t.cell_str(&cell(&t, 2, 0), &mut b), "👍\u{1f3fd}");
    assert!(cell(&t, 2, 0).is_wide());
    assert_eq!(t.cell_str(&cell(&t, 4, 0), &mut b), "👨\u{200d}👩\u{200d}👧");
    assert_eq!(cur(&t), (7, 0));
    feed(&mut t, "\r\n❤\u{fe0f}.");
    assert!(cell(&t, 0, 1).is_wide());
    assert_eq!(cur(&t), (3, 1));
}

#[test]
fn autowrap_pending() {
    let mut t = term(5, 3);
    feed(&mut t, "abcde");
    assert_eq!(cur(&t), (4, 0));
    assert!(t.cur.pending);
    feed(&mut t, "\r");
    assert_eq!(cur(&t), (0, 0));
    assert!(!t.screen()[0].wrapped);
    feed(&mut t, "\x1b[1;5HZ\x1b[6n");
    assert_eq!(t.take_responses(), b"\x1b[1;5R");
    feed(&mut t, "f");
    assert_eq!(screen(&t), ["abcdZ", "f", ""]);
    assert!(t.screen()[0].wrapped);
    feed(&mut t, "\x1b[?7l\x1b[3;1Hvwxyz12");
    assert_eq!(row_text(&t, 2), "vwxy2");
    feed(&mut t, "\x1b[?7h\x1b[1;1Habcde\x08X");
    assert_eq!(row_text(&t, 0), "abcXe");
}

#[test]
fn scroll_region_il_dl() {
    let mut t = term(4, 6);
    feed(&mut t, "0\r\n1\r\n2\r\n3\r\n4\r\n5");
    feed(&mut t, "\x1b[2;5r\x1b[3;1H\x1b[2L");
    assert_eq!(screen(&t), ["0", "1", "", "", "2", "5"]);
    feed(&mut t, "\x1b[2M");
    assert_eq!(screen(&t), ["0", "1", "2", "", "", "5"]);
    feed(&mut t, "\x1b[5;1H\nX\n\nY");
    assert_eq!(screen(&t), ["0", "", "X", "", " Y", "5"]);
    feed(&mut t, "\x1b[2;1H\x1bM\x1bMZ");
    assert_eq!(screen(&t), ["0", "Z", "", "", "X", "5"]);
    assert_eq!(t.scrollback_len(), 0);
    feed(&mut t, "\x1b[?6h\x1b[1;1HO\x1b[6n\x1b[?6l\x1b[r");
    assert_eq!(row_text(&t, 1), "O");
    assert_eq!(t.take_responses(), b"\x1b[1;2R");
    feed(&mut t, "\x1b[2S");
    assert_eq!(screen(&t), ["", "", "X", "5", "", ""]);
    assert_eq!(t.scrollback_len(), 2);
    feed(&mut t, "\x1b[T");
    assert_eq!(screen(&t), ["", "", "", "X", "5", ""]);
}

#[test]
fn alt_screen_1049() {
    let mut t = term(10, 3);
    feed(&mut t, "main\r\nline\x1b[1;31m");
    let before = (screen(&t), cur(&t));
    feed(&mut t, "\x1b[?1049h");
    assert!(t.is_alt_screen());
    assert_eq!(screen(&t), ["", "", ""]);
    feed(&mut t, "\x1b[Halt\r\n\r\n\r\n\r\nxx");
    assert_eq!(t.scrollback_len(), 0);
    feed(&mut t, "\x1b[?1049l");
    assert!(!t.is_alt_screen());
    assert_eq!((screen(&t), cur(&t)), before);
    assert_eq!(t.cur.pen.fg, Color::indexed(1));
    assert!(t.alt.is_empty());
    feed(&mut t, "\x1b[?1049h\x1b[?1049$p\x1b[?1049l\x1b[?1049$p\x1b[?4242$p");
    assert_eq!(t.take_responses(), b"\x1b[?1049;1$y\x1b[?1049;2$y\x1b[?4242;0$y");
}

#[test]
fn erase_display_and_line() {
    let mut t = term(5, 3);
    feed(&mut t, "aaaaabbbbbccccc\x1b[2;3H\x1b[K");
    assert_eq!(screen(&t), ["aaaaa", "bb", "ccccc"]);
    feed(&mut t, "\x1b[1K");
    assert_eq!(screen(&t), ["aaaaa", "", "ccccc"]);
    feed(&mut t, "\x1b[1;3H\x1b[0J");
    assert_eq!(screen(&t), ["aa", "", ""]);
    feed(&mut t, "\x1b[3;1Hzzz\x1b[3;2H\x1b[1J");
    assert_eq!(screen(&t), ["", "", "  z"]);
    feed(&mut t, "\x1b[44m\x1b[2J");
    assert_eq!(cell(&t, 0, 0).bg, Color::indexed(4));
    feed(&mut t, "\x1b[m\x1b[Habcde\x1b[1;2H\x1b[2X");
    assert_eq!(row_text(&t, 0), "a  de");
    feed(&mut t, "\x1b[2K");
    assert_eq!(row_text(&t, 0), "");
}

#[test]
fn replies() {
    let mut t = term(80, 24);
    t.set_cell_size(9, 18);
    feed(&mut t, "\x1b[c\x1b[>c\x1b[5n\x1b[3;7H\x1b[6n\x1b[?u\x1b[?2026$p\x1b[?2004h\x1b[?2004$p\x1b[4$p\x1b[18t\x1b[16t\x1b[14t");
    let r = String::from_utf8(t.take_responses()).unwrap();
    assert_eq!(
        r,
        "\x1b[?62;22c\x1b[>1;10;0c\x1b[0n\x1b[3;7R\x1b[?0u\x1b[?2026;2$y\x1b[?2004;1$y\x1b[4;2$y\x1b[8;24;80t\x1b[6;18;9t\x1b[4;432;720t"
    );
    feed(&mut t, "\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]4;1;?\x07\x1b]4;1;#102030\x07\x1b]4;1;?\x07\x1b]10;rgb:ff/80/00\x07\x1b]10;?\x07");
    let r = String::from_utf8(t.take_responses()).unwrap();
    assert_eq!(
        r,
        "\x1b]10;rgb:e5e5/e5e5/e5e5\x07\x1b]11;rgb:0000/0000/0000\x1b\\\x1b]4;1;rgb:cdcd/0000/0000\x07\x1b]4;1;rgb:1010/2020/3030\x07\x1b]10;rgb:ffff/8080/0000\x07"
    );
    feed(&mut t, "\x1b]104\x07\x1b]110\x07");
    assert_eq!(t.palette()[1], Rgb::new(0xcd, 0, 0));
    assert_eq!(t.default_colors().0, Theme::default().fg);
    feed(&mut t, "\x1b[>q");
    assert!(t.take_responses().starts_with(b"\x1bP>|davide-term"));
}

#[test]
fn osc_title_cwd_clipboard_links() {
    let mut t = term(20, 2);
    feed(&mut t, "\x1b]2;hello\x1b\\");
    assert!(t.take_title_changed());
    assert!(!t.take_title_changed());
    assert_eq!(t.title(), "hello");
    feed(&mut t, "\x1b[22t\x1b]0;other\x07\x1b[23t");
    assert_eq!(t.title(), "hello");
    feed(&mut t, "\x1b]7;file://host/home/d/a%20b\x07");
    assert_eq!(t.cwd(), "/home/d/a b");
    feed(&mut t, "\x1b]52;c;aGVsbG8g8J+YgA==\x07");
    assert_eq!(t.take_clipboard().as_deref(), Some("hello 😀"));
    feed(&mut t, "\x1b]52;c;?\x07");
    assert_eq!(t.take_clipboard(), None);
    feed(&mut t, "\x1b]8;id=a;http://a\x1b\\A\x1b]8;;\x1b\\B\x1b]8;;http://b\x07C\x1b]8;;\x07\x1b]8;id=a;http://a\x07D");
    let id = cell(&t, 0, 0).link;
    assert_eq!(t.link(id), Some("http://a"));
    assert_eq!(cell(&t, 1, 0).link, 0);
    assert_eq!(t.link(cell(&t, 2, 0).link), Some("http://b"));
    assert_eq!(cell(&t, 3, 0).link, id);
    feed(&mut t, "\x07\x07");
    assert_eq!(t.take_bell(), 2);
}

#[test]
fn bracketed_paste_and_focus() {
    let mut t = term(10, 2);
    assert_eq!(t.encode_paste("a\nb\r\nc"), b"a\rb\rc");
    assert_eq!(t.encode_focus(true), None);
    feed(&mut t, "\x1b[?2004h\x1b[?1004h");
    assert_eq!(t.encode_paste("x\x1b[201~y\n"), b"\x1b[200~xy\n\x1b[201~");
    assert_eq!(t.encode_focus(true), Some(&b"\x1b[I"[..]));
    assert_eq!(t.encode_focus(false), Some(&b"\x1b[O"[..]));
}

#[test]
fn key_encoding() {
    let mut t = term(10, 2);
    let none = Mods::default();
    let ctrl = Mods { ctrl: true, ..none };
    let alt = Mods { alt: true, ..none };
    let shift = Mods { shift: true, ..none };
    let cs = Mods { ctrl: true, shift: true, ..none };
    let k = |t: &Terminal, k, m| t.encode_key(k, m).unwrap();
    let cases: &[(Key, Mods, &[u8])] = &[
        (Key::Char('a'), none, b"a"),
        (Key::Char('é'), none, "é".as_bytes()),
        (Key::Char('a'), ctrl, b"\x01"),
        (Key::Char('Z'), ctrl, b"\x1a"),
        (Key::Char(' '), ctrl, b"\x00"),
        (Key::Char('@'), ctrl, b"\x00"),
        (Key::Char('['), ctrl, b"\x1b"),
        (Key::Char('_'), ctrl, b"\x1f"),
        (Key::Char('x'), alt, b"\x1bx"),
        (Key::Char('c'), Mods { alt: true, ctrl: true, shift: false }, b"\x1b\x03"),
        (Key::Enter, none, b"\r"),
        (Key::Enter, alt, b"\x1b\r"),
        (Key::Tab, none, b"\t"),
        (Key::Tab, shift, b"\x1b[Z"),
        (Key::Backspace, none, b"\x7f"),
        (Key::Backspace, ctrl, b"\x08"),
        (Key::Backspace, alt, b"\x1b\x7f"),
        (Key::Escape, none, b"\x1b"),
        (Key::Up, none, b"\x1b[A"),
        (Key::Left, ctrl, b"\x1b[1;5D"),
        (Key::Right, cs, b"\x1b[1;6C"),
        (Key::Home, none, b"\x1b[H"),
        (Key::End, shift, b"\x1b[1;2F"),
        (Key::PageUp, none, b"\x1b[5~"),
        (Key::PageDown, alt, b"\x1b[6;3~"),
        (Key::Insert, none, b"\x1b[2~"),
        (Key::Delete, ctrl, b"\x1b[3;5~"),
        (Key::F(1), none, b"\x1bOP"),
        (Key::F(4), shift, b"\x1b[1;2S"),
        (Key::F(5), none, b"\x1b[15~"),
        (Key::F(12), ctrl, b"\x1b[24;5~"),
    ];
    for (key, m, want) in cases {
        assert_eq!(k(&t, *key, *m), *want, "{key:?} {m:?}");
    }
    assert_eq!(t.encode_key(Key::F(13), none), None);
    feed(&mut t, "\x1b[?1h\x1b[20h");
    assert_eq!(k(&t, Key::Up, none), b"\x1bOA");
    assert_eq!(k(&t, Key::Home, none), b"\x1bOH");
    assert_eq!(k(&t, Key::Up, shift), b"\x1b[1;2A");
    assert_eq!(k(&t, Key::Enter, none), b"\r\n");
}

#[test]
fn mouse_encoding() {
    let mut t = term(300, 50);
    let ev = |action, col, row| MouseEvent { action, col, row, mods: Mods::default() };
    let press = ev(MouseAction::Press(MouseButton::Left), 4, 2);
    assert!(!t.wants_mouse());
    assert_eq!(t.encode_mouse(press), None);
    feed(&mut t, "\x1b[?1000h\x1b[?1006h");
    assert!(t.wants_mouse());
    assert_eq!(t.modes().mouse, MouseMode::Click);
    assert_eq!(t.encode_mouse(press).unwrap(), b"\x1b[<0;5;3M");
    assert_eq!(t.encode_mouse(ev(MouseAction::Release(MouseButton::Left), 4, 2)).unwrap(), b"\x1b[<0;5;3m");
    assert_eq!(t.encode_mouse(ev(MouseAction::Motion(Some(MouseButton::Left)), 5, 2)), None);
    let wheel = MouseEvent { mods: Mods { ctrl: true, ..Mods::default() }, ..ev(MouseAction::Press(MouseButton::WheelDown), 0, 0) };
    assert_eq!(t.encode_mouse(wheel).unwrap(), b"\x1b[<81;1;1M");
    assert_eq!(t.encode_mouse(ev(MouseAction::Release(MouseButton::WheelDown), 0, 0)), None);
    feed(&mut t, "\x1b[?1002h");
    assert_eq!(t.encode_mouse(ev(MouseAction::Motion(Some(MouseButton::Right)), 7, 3)).unwrap(), b"\x1b[<34;8;4M");
    assert_eq!(t.encode_mouse(ev(MouseAction::Motion(Some(MouseButton::Right)), 7, 3)), None);
    assert_eq!(t.encode_mouse(ev(MouseAction::Motion(None), 8, 3)), None);
    feed(&mut t, "\x1b[?1003h");
    assert_eq!(t.encode_mouse(ev(MouseAction::Motion(None), 8, 3)).unwrap(), b"\x1b[<35;9;4M");
    feed(&mut t, "\x1b[?1006l");
    assert_eq!(t.encode_mouse(ev(MouseAction::Press(MouseButton::Middle), 1, 1)).unwrap(), b"\x1b[M!\"\"");
    assert_eq!(t.encode_mouse(ev(MouseAction::Release(MouseButton::Middle), 1, 1)).unwrap(), b"\x1b[M#\"\"");
    assert_eq!(t.encode_mouse(ev(MouseAction::Press(MouseButton::Left), 250, 1)).unwrap(), b"\x1b[M \xff\"");
    feed(&mut t, "\x1b[?1003l");
    assert!(!t.wants_mouse());
    feed(&mut t, "\x1b[?1049h");
    assert_eq!(t.encode_mouse(ev(MouseAction::Press(MouseButton::WheelUp), 0, 0)).unwrap(), b"\x1b[A");
}

#[test]
fn scrollback_round_trip() {
    let mut t = term(20, 3);
    feed(&mut t, "plain line\r\n\x1b[1;38;2;1;2;3mbold\x1b[m mid \x1b[44mblue\x1b[m\r\n");
    feed(&mut t, "日本 e\u{301} 👍\u{1f3fd} ❤\u{fe0f}\r\n\x1b]8;;http://z\x07ln\x1b]8;;\x07\r\n");
    feed(&mut t, "abcdefghijklmnopqrstuvwxyz\r\n\x1b[41m\x1b[K\x1b[m\r\n\r\n");
    let mut rows = Vec::new();
    let mut t2 = term(20, 200);
    t2.feed(b"plain line\r\n\x1b[1;38;2;1;2;3mbold\x1b[m mid \x1b[44mblue\x1b[m\r\n");
    t2.feed("日本 e\u{301} 👍\u{1f3fd} ❤\u{fe0f}\r\n\x1b]8;;http://z\x07ln\x1b]8;;\x07\r\n".as_bytes());
    t2.feed(b"abcdefghijklmnopqrstuvwxyz\r\n\x1b[41m\x1b[K\x1b[m\r\n\r\n");
    for y in 0..7 {
        rows.push((t2.screen()[y].cells.clone(), t2.screen()[y].wrapped));
    }
    assert_eq!(t.scrollback_len(), 6);
    let mut buf = Vec::new();
    for (i, (cells, wrapped)) in rows.iter().take(6).enumerate() {
        let w = crate::scrollback::decode(&t.sb.lines[i], 20, &t.clusters, &mut buf);
        let norm = |c: &[Cell]| c.iter().map(|c| Cell { link: (c.link != 0) as u16, ..*c }).collect::<Vec<_>>();
        assert_eq!(norm(&buf), norm(cells), "line {i}");
        assert_eq!(w, *wrapped, "line {i}");
    }
    t.scroll_view(2);
    assert_eq!(row_text(&t, 0), "abcdefghijklmnopqrst");
    assert!(t.visible_row_wrapped(0));
    assert_eq!(row_text(&t, 1), "uvwxyz");
    let mut b = Vec::new();
    assert_eq!(t.visible_row(2, &mut b)[0].bg, Color::indexed(1));
}

#[test]
fn scrollback_memory() {
    let mut t = Terminal::new(120, 40, 10_000, Theme::default());
    let mut line = String::new();
    for i in 0..12_000 {
        line.clear();
        line.push_str(&format!("\x1b[32m{i:05}\x1b[m drwxr-xr-x  2 davide users 4096 Nov  6 12:00 \x1b[1;34msome-directory\x1b[0m\r\n"));
        t.feed(line.as_bytes());
    }
    assert_eq!(t.scrollback_len(), 10_000);
    let bytes = t.scrollback_bytes();
    let per_line = bytes / 10_000;
    eprintln!("scrollback: {bytes} bytes, {per_line} per line, cell {} bytes", std::mem::size_of::<Cell>());
    assert!(bytes < 2 << 20, "{bytes}");
    assert!(per_line < 120);
    assert_eq!(std::mem::size_of::<Cell>(), 16);
    let mut plain = Terminal::new(120, 40, 10_000, Theme::default());
    let s = "x".repeat(60) + "\r\n";
    for _ in 0..10_040 {
        plain.feed(s.as_bytes());
    }
    eprintln!("plain 60-char lines: {} bytes", plain.scrollback_bytes());
    assert!(plain.scrollback_bytes() < 1 << 20);
}

#[test]
fn view_anchored_and_damage() {
    let mut t = term(10, 3);
    for i in 0..10 {
        feed(&mut t, &format!("{i}\r\n"));
    }
    let _ = t.take_damage();
    assert!(t.take_damage().is_empty());
    feed(&mut t, "x");
    let d = t.take_damage();
    assert!(!d.full && d.contains(2) && !d.contains(0));
    assert_eq!(d.rows().collect::<Vec<_>>(), [2]);
    t.scroll_view(3);
    assert!(t.take_damage().full);
    assert_eq!(row_text(&t, 0), "5");
    assert!(!t.cursor().visible);
    feed(&mut t, "\r\nnew\r\n");
    assert_eq!(t.view_offset(), 5);
    assert_eq!(row_text(&t, 0), "5");
    t.scroll_view(1000);
    assert_eq!(t.view_offset(), t.scrollback_len());
    assert_eq!(row_text(&t, 0), "0");
    t.reset_view();
    assert_eq!(t.view_offset(), 0);
    assert_eq!(row_text(&t, 1), "new");
    let _ = t.take_damage();
    feed(&mut t, "\x1b[1;1H");
    let d = t.take_damage();
    assert!(d.contains(0) && d.contains(2));
}

#[test]
fn resize_behaviour() {
    let mut t = term(10, 5);
    feed(&mut t, "a\r\nb\r\nc\r\nd\r\ne");
    t.resize(10, 3);
    assert_eq!(screen(&t), ["c", "d", "e"]);
    assert_eq!(t.scrollback_len(), 2);
    assert_eq!(cur(&t), (1, 2));
    t.resize(10, 6);
    assert_eq!(screen(&t), ["a", "b", "c", "d", "e", ""]);
    assert_eq!(cur(&t), (1, 4));
    assert_eq!(t.scrollback_len(), 0);
    feed(&mut t, "\x1b[1;1H");
    t.resize(4, 2);
    assert_eq!(screen(&t), ["a", "b"]);
    assert_eq!(cur(&t), (0, 0));
    let mut t = term(6, 2);
    feed(&mut t, "abcd日\x1b[?1049h\x1b[2;5Hx");
    t.resize(5, 4);
    assert_eq!(screen(&t), ["", "    x", "", ""]);
    feed(&mut t, "\x1b[?1049l");
    assert_eq!(screen(&t), ["abcd", "", "", ""]);
    t.resize(8, 4);
    feed(&mut t, "\x1b[1;1H\t\tX");
    assert_eq!(cur(&t), (7, 0));
    assert!(t.take_damage().full);
}

#[test]
fn selection_text() {
    let mut t = term(5, 6);
    feed(&mut t, "hello world\r\nab  \r\nfoo bar");
    assert_eq!(screen(&t), ["hello", " worl", "d", "ab", "foo b", "ar"]);
    let p = |l, c| Point { line: l, col: c };
    t.set_selection(t.point_at(1, 0), t.point_at(1, 3), SelectionMode::Char);
    assert_eq!(t.selected_text(), "ello world\nab");
    assert!(t.is_selected(4, 0) && !t.is_selected(0, 0) && t.is_selected(0, 3) && !t.is_selected(2, 3));
    t.set_selection(t.point_at(3, 3), t.point_at(4, 3), SelectionMode::Char);
    assert_eq!(t.selected_text(), "");
    t.set_selection(t.point_at(2, 1), t.point_at(2, 1), SelectionMode::Word);
    assert_eq!(t.selected_text(), "worl");
    t.set_selection(t.point_at(0, 1), t.point_at(0, 1), SelectionMode::Line);
    assert_eq!(t.selected_text(), "hello world");
    feed(&mut t, "\r\nX\r\nY");
    assert_eq!(t.scrollback_len(), 2);
    assert_eq!(t.selection().map(|s| s.0), Some(p(0, 0)));
    assert_eq!(t.selected_text(), "hello world");
    t.scroll_view(2);
    assert!(t.is_selected(0, 0));
    t.reset_view();
    t.set_selection(t.point_at(0, 3), t.point_at(0, 3), SelectionMode::Char);
    feed(&mut t, "\x1b[4;1Hz");
    assert_eq!(t.selection(), None);
    t.set_selection(t.point_at(0, 0), t.point_at(0, 0), SelectionMode::Char);
    feed(&mut t, "\x1b[4;1Hq");
    assert!(t.selection().is_some());
    t.clear_selection();
    assert_eq!(t.selected_text(), "");
}

#[test]
fn persist_round_trip() {
    let mut t = term(30, 8);
    t.feed(&sample());
    feed(&mut t, "\x1b[?1h\x1b[?2004h\x1b[5 q\x1b]4;3;#010203\x07\x1b]7;file:///tmp\x07\x1b[?1049h\x1b[2;2Halt\x1b[3;4r");
    let _ = t.take_responses();
    let mut e = state::Enc::default();
    state::Persist::save(&t, &mut e);
    let mut d = state::Dec::new(&e.buf);
    let mut u: Terminal = state::Persist::load(&mut d).unwrap();
    assert!(d.done());
    assert_eq!(snapshot(&t), snapshot(&u));
    assert_eq!(t.modes(), u.modes());
    assert_eq!((t.cwd(), t.palette()[3], t.cursor()), (u.cwd(), u.palette()[3], u.cursor()));
    assert_eq!(u.modes().cursor_shape, CursorShape::Bar);
    feed(&mut t, "\x1b[?1049lmore");
    feed(&mut u, "\x1b[?1049lmore");
    assert_eq!(snapshot(&t), snapshot(&u));
    assert!(u.clusters.find("e\u{301}").is_some());
    let mut d = state::Dec::new(&e.buf[..e.buf.len() / 2]);
    assert!(<Terminal as state::Persist>::load(&mut d).is_err());
}

/// Ink redraws its dynamic region: hide the cursor, move up over the old
/// frame erasing each line, rewrite, all inside a synchronized update.
#[test]
fn claude_code_redraw() {
    let mut t = term(40, 10);
    feed(&mut t, "$ claude\r\n");
    let frame = |lines: &[&str], prev: usize| {
        let mut s = String::from("\x1b[?2026h\x1b[?25l");
        for i in 0..prev {
            s.push_str("\x1b[2K");
            if i + 1 < prev {
                s.push_str("\x1b[1A");
            }
        }
        if prev > 0 {
            s.push_str("\x1b[G");
        }
        for (i, l) in lines.iter().enumerate() {
            s.push_str(l);
            if i + 1 < lines.len() {
                s.push_str("\r\n");
            }
        }
        s.push_str("\x1b[?25h\x1b[?2026l");
        s
    };
    let box_ = |w: &str| ["\x1b[38;5;244m╭──────────╮\x1b[39m".to_string(), format!("\x1b[38;5;244m│\x1b[39m \x1b[1m{w:<8}\x1b[22m \x1b[38;5;244m│\x1b[39m"), "\x1b[38;5;244m╰──────────╯\x1b[39m".to_string()];
    let f1 = box_("hi");
    let f1: Vec<&str> = f1.iter().map(String::as_str).collect();
    let mut first = frame(&f1, 0);
    first.insert(0, '\r');
    feed(&mut t, &first);
    let mut sync_seen = false;
    let f2 = box_("thinking");
    let f2: Vec<&str> = f2.iter().map(String::as_str).chain(["\x1b[2m✻ Working…\x1b[22m"]).collect();
    for b in frame(&f2, 3).as_bytes() {
        t.feed(std::slice::from_ref(b));
        sync_seen |= t.sync();
    }
    assert!(sync_seen && !t.sync());
    assert_eq!(screen(&t)[..6], ["$ claude", "╭──────────╮", "│ thinking │", "╰──────────╯", "✻ Working…", ""]);
    assert!(t.cursor().visible);
    assert_eq!(cell(&t, 2, 2).attrs, attr::BOLD);
    assert_eq!(cell(&t, 0, 1).fg, Color::indexed(244));
    assert_eq!(cell(&t, 0, 4).attrs, attr::DIM);
    let f3 = ["\x1b[32m●\x1b[39m done"];
    feed(&mut t, &frame(&f3, 4));
    assert_eq!(screen(&t)[..4], ["$ claude", "● done", "", ""]);
    assert_eq!(cur(&t), (6, 1));
    feed(&mut t, "\x1b[?2026$p");
    assert_eq!(t.take_responses(), b"\x1b[?2026;2$y");
}

#[test]
fn tabs_and_charsets() {
    let mut t = term(30, 2);
    feed(&mut t, "a\tb\x1b[3gc\td\x1b[1;5H\x1bH\x1b[1;1H\te");
    assert_eq!(row_text(&t, 0), "a   e   bc                   d");
    feed(&mut t, "\x1b[2;1H\x1b(0lqqk\x1b(Bq\x0e\x0f");
    assert_eq!(row_text(&t, 1), "┌──┐q");
    feed(&mut t, "\x1b)0\x0eq\x0fq");
    assert_eq!(row_text(&t, 1), "┌──┐q─q");
}

#[test]
fn reset_and_rep() {
    let mut t = term(10, 3);
    feed(&mut t, "ab\x1b[3bX\x1b[?25l\x1b[4h");
    assert_eq!(row_text(&t, 0), "abbbbX");
    feed(&mut t, "\x1b[1;1HZ");
    assert_eq!(row_text(&t, 0), "ZabbbbX");
    feed(&mut t, "\x1bc");
    assert_eq!(screen(&t), ["", "", ""]);
    assert!(t.modes().cursor_visible && !t.modes().insert);
    feed(&mut t, "\x1b#8");
    assert_eq!(row_text(&t, 2), "EEEEEEEEEE");
}
