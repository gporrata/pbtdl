//! Terminal result presentation and deterministic selection.

use crate::model::{TorrentResult, sanitize_display_text};
use anyhow::{Context, Result, bail};
use console::{Key, Term, measure_text_width, style, truncate_str};
use std::io;

pub trait ResultChooser {
    fn choose(&mut self, results: &[TorrentResult], automatic: bool) -> Result<usize>;
}

#[derive(Debug, Clone)]
pub struct TerminalChooser {
    max_title_characters: usize,
}

impl TerminalChooser {
    pub fn new(max_title_characters: usize) -> Self {
        Self {
            max_title_characters,
        }
    }
}

impl ResultChooser for TerminalChooser {
    fn choose(&mut self, results: &[TorrentResult], automatic: bool) -> Result<usize> {
        if results.is_empty() {
            bail!("no eligible torrent results were found");
        }
        if automatic {
            return Ok(0);
        }
        let items: Vec<_> = results
            .iter()
            .map(|result| format_result(result, self.max_title_characters))
            .collect();
        interact(&items)?.ok_or_else(|| anyhow::anyhow!("torrent selection was cancelled"))
    }
}

pub fn format_result(result: &TorrentResult, max_title_characters: usize) -> String {
    let title = truncate_with_ellipsis(
        &sanitize_display_text(&result.name, max_title_characters.saturating_add(1)),
        max_title_characters,
    );
    let seeders = result
        .seeders
        .map_or_else(|| "?".to_string(), |value| value.to_string());
    let leechers = result
        .leechers
        .map_or_else(|| "?".to_string(), |value| value.to_string());
    let category = result
        .category
        .as_deref()
        .map(|value| sanitize_display_text(value, 128))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let source = sanitize_display_text(&result.source_host, 253);
    let size = result
        .size_bytes
        .map_or_else(|| "unknown".to_string(), |value| human_size(Some(value)));
    format!("{title} | seeders {seeders} | leechers {leechers} | {category} | {source} | {size}")
}

pub fn human_size(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "unknown".to_string();
    };
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for candidate in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = candidate;
    }
    if unit == "B" {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {unit}")
    }
}

pub fn truncate_with_ellipsis(value: &str, max_characters: usize) -> String {
    let count = value.chars().count();
    if count <= max_characters {
        return value.to_string();
    }
    if max_characters == 0 {
        return String::new();
    }
    if max_characters == 1 {
        return "…".to_string();
    }
    let mut truncated: String = value.chars().take(max_characters - 1).collect();
    truncated.push('…');
    truncated
}

/// Rows of result text that fit under the prompt.
///
/// One row is reserved for the prompt and one is left empty. Writing into
/// that last row makes the terminal scroll, and the next redraw then appends
/// a new copy of the list instead of replacing it.
fn visible_row_count(term_rows: usize, item_count: usize) -> usize {
    if item_count == 0 {
        return 0;
    }
    let room = term_rows.saturating_sub(2).max(1);
    room.min(item_count)
}

/// Columns available for one menu row.
///
/// Staying one column short of the terminal width avoids the terminals that
/// wrap as soon as the cursor reaches the last column, before the newline.
fn line_budget(term_columns: usize) -> usize {
    term_columns.saturating_sub(1).max(1)
}

fn fit_columns(value: &str, max_columns: usize) -> String {
    if max_columns == 0 {
        return String::new();
    }
    truncate_str(value, max_columns, "…").into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollList {
    selected: usize,
    offset: usize,
}

impl ScrollList {
    fn new() -> Self {
        Self {
            selected: 0,
            offset: 0,
        }
    }

    fn move_by(&mut self, delta: isize, len: usize, visible: usize) {
        if len == 0 {
            return;
        }
        if delta < 0 {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected = self.selected.saturating_add(delta as usize).min(len - 1);
        }
        self.reveal(len, visible);
    }

    fn jump(&mut self, index: usize, len: usize, visible: usize) {
        if len == 0 {
            return;
        }
        self.selected = index.min(len - 1);
        self.reveal(len, visible);
    }

    fn reveal(&mut self, len: usize, visible: usize) {
        if len == 0 {
            self.selected = 0;
            self.offset = 0;
            return;
        }
        let visible = visible.clamp(1, len);
        self.selected = self.selected.min(len - 1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
        let max_offset = len - visible;
        if self.offset > max_offset {
            self.offset = max_offset;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Redraw,
    Accept,
    Cancel,
}

fn page_step(visible: usize) -> usize {
    visible.saturating_sub(1).max(1)
}

fn apply_key(list: &mut ScrollList, key: Key, len: usize, visible: usize) -> KeyAction {
    match key {
        Key::ArrowUp | Key::Char('k') | Key::BackTab => {
            list.move_by(-1, len, visible);
            KeyAction::Redraw
        }
        Key::ArrowDown | Key::Char('j') | Key::Tab => {
            list.move_by(1, len, visible);
            KeyAction::Redraw
        }
        Key::PageUp | Key::ArrowLeft => {
            let step = page_step(visible) as isize;
            list.move_by(-step, len, visible);
            KeyAction::Redraw
        }
        Key::PageDown | Key::ArrowRight => {
            let step = page_step(visible) as isize;
            list.move_by(step, len, visible);
            KeyAction::Redraw
        }
        Key::Home => {
            list.jump(0, len, visible);
            KeyAction::Redraw
        }
        Key::End => {
            list.jump(len.saturating_sub(1), len, visible);
            KeyAction::Redraw
        }
        Key::Enter | Key::Char(' ') => KeyAction::Accept,
        Key::Escape | Key::Char('q') | Key::CtrlC => KeyAction::Cancel,
        _ => KeyAction::Redraw,
    }
}

fn prompt_text(list: &ScrollList, len: usize, visible: usize) -> String {
    let end = list.offset.saturating_add(visible).min(len);
    let more = match (list.offset > 0, end < len) {
        (true, true) => " ↑↓",
        (true, false) => " ↑",
        (false, true) => " ↓",
        (false, false) => "",
    };
    format!(
        "Select a torrent ({}/{}){more}",
        list.selected.saturating_add(1),
        len
    )
}

fn menu_lines(
    items: &[String],
    list: &ScrollList,
    term_columns: usize,
    visible: usize,
) -> Vec<String> {
    let budget = line_budget(term_columns);
    let mut lines = Vec::with_capacity(visible.saturating_add(1));
    let prompt = fit_columns(&prompt_text(list, items.len(), visible), budget);
    lines.push(style(prompt).bold().for_stderr().to_string());

    for (index, item) in items.iter().enumerate().skip(list.offset).take(visible) {
        lines.push(item_line(item, index == list.selected, budget));
    }
    lines
}

fn item_line(text: &str, active: bool, budget: usize) -> String {
    let prefix = if active { "❯ " } else { "  " };
    let prefix_width = measure_text_width(prefix);
    let body = fit_columns(text, budget.saturating_sub(prefix_width));
    let plain = format!("{prefix}{body}");
    let plain = if measure_text_width(&plain) > budget {
        fit_columns(&plain, budget)
    } else {
        plain
    };
    if active {
        style(plain).cyan().bold().for_stderr().to_string()
    } else {
        plain
    }
}

struct MenuGuard<'a> {
    term: &'a Term,
    lines: usize,
}

impl<'a> MenuGuard<'a> {
    fn hide(term: &'a Term) -> io::Result<Self> {
        term.hide_cursor()?;
        Ok(Self { term, lines: 0 })
    }
}

impl Drop for MenuGuard<'_> {
    fn drop(&mut self) {
        if self.lines > 0 {
            let _ = self.term.clear_last_lines(self.lines);
        }
        let _ = self.term.show_cursor();
        let _ = self.term.flush();
    }
}

fn paint(term: &Term, lines: &[String], previous: usize) -> io::Result<usize> {
    if previous > 0 {
        term.clear_last_lines(previous)?;
    }
    for line in lines {
        term.write_line(line)?;
    }
    term.flush()?;
    Ok(lines.len())
}

fn interact(items: &[String]) -> Result<Option<usize>> {
    let term = Term::stderr();
    if !term.is_term() {
        bail!("interactive selection requires a terminal");
    }
    let mut list = ScrollList::new();
    let mut guard = MenuGuard::hide(&term).context("interactive selection failed")?;
    loop {
        let (rows, columns) = term.size();
        let visible = visible_row_count(rows as usize, items.len());
        list.reveal(items.len(), visible);
        let lines = menu_lines(items, &list, columns as usize, visible);
        guard.lines = paint(&term, &lines, guard.lines).context("interactive selection failed")?;
        let key = term.read_key().context("interactive selection failed")?;
        match apply_key(&mut list, key, items.len(), visible) {
            KeyAction::Redraw => {}
            KeyAction::Accept => return Ok(Some(list.selected)),
            KeyAction::Cancel => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MagnetUri;
    use std::str::FromStr;

    fn result() -> TorrentResult {
        TorrentResult {
            name: "Ubuntu 🐧 résumé image".to_string(),
            magnet: MagnetUri::from_str(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            )
            .expect("magnet"),
            seeders: Some(42),
            leechers: None,
            size_bytes: Some(1_610_612_736),
            category: Some("Applications".to_string()),
            source_host: "proxy.example".to_string(),
        }
    }

    #[test]
    fn formats_all_available_and_unknown_fields() {
        let formatted = format_result(&result(), 14);

        assert!(formatted.contains("Ubuntu 🐧 résu…"));
        assert!(formatted.contains("seeders 42"));
        assert!(formatted.contains("leechers ?"));
        assert!(formatted.contains("Applications"));
        assert!(formatted.contains("proxy.example"));
        assert!(formatted.contains("1.50 GiB"));
    }

    #[test]
    fn unicode_truncation_is_safe_at_small_limits() {
        assert_eq!(truncate_with_ellipsis("🦀Rust", 3), "🦀R…");
        assert_eq!(truncate_with_ellipsis("🦀", 1), "🦀");
        assert_eq!(truncate_with_ellipsis("🦀Rust", 1), "…");
        assert_eq!(truncate_with_ellipsis("🦀Rust", 0), "");
    }

    #[test]
    fn formats_human_sizes() {
        assert_eq!(human_size(None), "unknown");
        assert_eq!(human_size(Some(512)), "512 B");
        assert_eq!(human_size(Some(1_048_576)), "1.00 MiB");
    }

    #[test]
    fn automatic_selection_chooses_the_first_ranked_result() {
        let mut chooser = TerminalChooser::new(80);
        assert_eq!(chooser.choose(&[result()], true).expect("automatic"), 0);
    }

    #[test]
    fn formatting_does_not_emit_page_supplied_terminal_controls() {
        let mut hostile = result();
        hostile.name = "normal\u{1b}[2J title".to_string();
        hostile.category = Some("Apps\u{202e}evil".to_string());

        let formatted = format_result(&hostile, 80);

        assert!(!formatted.contains('\u{1b}'));
        assert!(!formatted.contains('\u{202e}'));
    }

    #[test]
    fn visible_window_leaves_room_for_the_prompt_and_a_spare_row() {
        assert_eq!(visible_row_count(24, 10), 10);
        assert_eq!(visible_row_count(10, 30), 8);
        assert_eq!(visible_row_count(2, 5), 1);
        assert_eq!(visible_row_count(40, 0), 0);
    }

    #[test]
    fn column_budget_stays_inside_the_terminal_width() {
        assert_eq!(line_budget(80), 79);
        assert_eq!(line_budget(1), 1);
        assert!(measure_text_width(&fit_columns(&"x".repeat(200), 20)) <= 20);
        assert!(measure_text_width(&fit_columns(&"🦀".repeat(20), 5)) <= 5);
        assert_eq!(fit_columns("short", 20), "short");
    }

    #[test]
    fn scrolling_keeps_one_fixed_window_of_single_rows() {
        let items: Vec<_> = (0..12)
            .map(|index| {
                let mut item = result();
                item.name = format!("Legal fixture {index} {}", "wide-🦀".repeat(20));
                format_result(&item, 120)
            })
            .collect();
        let mut list = ScrollList::new();
        let columns = 40;
        let visible = visible_row_count(8, items.len());
        assert_eq!(visible, 6);

        let mut previous_height = None;
        for _ in 0..=items.len() {
            list.reveal(items.len(), visible);
            let lines = menu_lines(&items, &list, columns, visible);
            assert_eq!(lines.len(), visible + 1);
            assert_eq!(lines.iter().filter(|line| line.contains('❯')).count(), 1);
            for line in &lines {
                assert!(!line.contains('\n'));
                assert!(measure_text_width(line) <= line_budget(columns));
            }
            let height = lines.len();
            if let Some(previous) = previous_height {
                assert_eq!(previous, height);
            }
            previous_height = Some(height);
            let window = list.offset..list.offset + visible;
            assert!(window.contains(&list.selected));
            list.move_by(1, items.len(), visible);
        }

        assert_eq!(list.selected, items.len() - 1);
        list.move_by(1, items.len(), visible);
        assert_eq!(list.selected, items.len() - 1);

        list.jump(0, items.len(), visible);
        assert_eq!(list.selected, 0);
        assert_eq!(list.offset, 0);
        list.move_by(-1, items.len(), visible);
        assert_eq!(list.selected, 0);
    }

    #[test]
    fn paging_and_vertical_keys_scroll_the_same_list() {
        let mut list = ScrollList::new();
        let len = 20;
        let visible = 5;

        assert_eq!(
            apply_key(&mut list, Key::ArrowDown, len, visible),
            KeyAction::Redraw
        );
        assert_eq!(list.selected, 1);
        apply_key(&mut list, Key::Char('j'), len, visible);
        assert_eq!(list.selected, 2);
        apply_key(&mut list, Key::PageDown, len, visible);
        assert_eq!(list.selected, 6);
        assert_eq!(list.offset, 2);
        apply_key(&mut list, Key::ArrowUp, len, visible);
        assert_eq!(list.selected, 5);
        apply_key(&mut list, Key::End, len, visible);
        assert_eq!(list.selected, 19);
        assert!((list.offset..list.offset + visible).contains(&list.selected));
        apply_key(&mut list, Key::Home, len, visible);
        assert_eq!((list.selected, list.offset), (0, 0));
        assert_eq!(
            apply_key(&mut list, Key::Enter, len, visible),
            KeyAction::Accept
        );
        assert_eq!(
            apply_key(&mut list, Key::Char('q'), len, visible),
            KeyAction::Cancel
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn redrawing_on_a_pty_scrolls_in_place() {
        let rows = 12u16;
        let columns = 40u16;
        let (mut master, slave) = open_pty(rows, columns).expect("open pty");
        let slave_read = slave.try_clone().expect("clone pty slave");
        let term = Term::read_write_pair(slave_read, slave);
        assert!(term.is_term());
        assert_eq!(term.size(), (rows, columns));

        let items: Vec<_> = (0..20)
            .map(|index| {
                let mut item = result();
                item.name = format!("ROW{index:02}-{}", "W".repeat(180));
                format_result(&item, 120)
            })
            .collect();
        let mut list = ScrollList::new();
        let visible = visible_row_count(rows as usize, items.len());
        let mut painted = 0usize;
        let mut captured = Vec::new();
        for _ in 0..12 {
            list.reveal(items.len(), visible);
            let lines = menu_lines(&items, &list, columns as usize, visible);
            assert_eq!(lines.len(), visible + 1);
            assert!(lines.len() < rows as usize);
            painted = paint(&term, &lines, painted).expect("paint menu");
            captured.extend(read_available(&mut master).expect("read pty"));
            list.move_by(1, items.len(), visible);
        }

        let screen = interpret_screen(
            &String::from_utf8_lossy(&captured),
            rows as usize,
            columns as usize,
        );
        let rendered = screen.text();
        assert_eq!(
            screen.scrolls, 0,
            "menu redraw scrolled the terminal:\n{rendered}"
        );
        assert_eq!(screen.wraps, 0, "menu row wrapped:\n{rendered}");
        assert_eq!(
            rendered.matches("Select a torrent").count(),
            1,
            "{rendered}"
        );
        assert!(!rendered.contains("ROW00"), "{rendered}");
        assert!(rendered.contains("ROW02"), "{rendered}");
        assert!(rendered.contains("ROW11"), "{rendered}");
    }

    #[cfg(target_os = "linux")]
    fn open_pty(rows: u16, columns: u16) -> std::io::Result<(std::fs::File, std::fs::File)> {
        use std::ffi::CStr;
        use std::fs::{File, OpenOptions};
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        const O_RDWR: i32 = 2;
        const O_NOCTTY: i32 = 0o400;
        const TIOCSWINSZ: u64 = 0x5414;

        #[repr(C)]
        struct WinSize {
            row: u16,
            col: u16,
            xpixel: u16,
            ypixel: u16,
        }

        unsafe extern "C" {
            fn posix_openpt(flags: i32) -> i32;
            fn grantpt(fd: i32) -> i32;
            fn unlockpt(fd: i32) -> i32;
            fn ptsname(fd: i32) -> *mut i8;
            fn ioctl(fd: i32, request: u64, ...) -> i32;
        }

        let master_fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
        if master_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        if unsafe { grantpt(master.as_raw_fd()) } != 0
            || unsafe { unlockpt(master.as_raw_fd()) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let name_ptr = unsafe { ptsname(master.as_raw_fd()) };
        if name_ptr.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let name = unsafe { CStr::from_ptr(name_ptr) }
            .to_string_lossy()
            .into_owned();
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(name)?;
        let size = WinSize {
            row: rows,
            col: columns,
            xpixel: 0,
            ypixel: 0,
        };
        if unsafe { ioctl(slave.as_raw_fd(), TIOCSWINSZ, &size) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((File::from(master), slave))
    }

    #[cfg(target_os = "linux")]
    fn read_available(file: &mut std::fs::File) -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        use std::os::unix::io::AsRawFd;

        const F_GETFL: i32 = 3;
        const F_SETFL: i32 = 4;
        const O_NONBLOCK: i32 = 0o4000;

        unsafe extern "C" {
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        }

        let fd = file.as_raw_fd();
        let flags = unsafe { fcntl(fd, F_GETFL) };
        if flags < 0 || unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut captured = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => captured.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(captured)
    }

    #[cfg(target_os = "linux")]
    struct Screen {
        rows: Vec<Vec<char>>,
        cursor_row: usize,
        cursor_col: usize,
        height: usize,
        width: usize,
        scrolls: usize,
        wraps: usize,
    }

    #[cfg(target_os = "linux")]
    impl Screen {
        fn text(&self) -> String {
            self.rows
                .iter()
                .map(|row| row.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn csi(&mut self, params: &str, command: char) {
            let amount = params
                .split([';', '?'])
                .find_map(|part| part.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1);
            match command {
                'A' => self.cursor_row = self.cursor_row.saturating_sub(amount),
                'B' => {
                    self.cursor_row = (self.cursor_row + amount).min(self.height.saturating_sub(1));
                }
                'K' if params.contains('2') => {
                    self.ensure_row();
                    self.rows[self.cursor_row].clear();
                }
                _ => {}
            }
        }

        fn newline(&mut self) {
            self.cursor_col = 0;
            if self.cursor_row + 1 >= self.height {
                self.scrolls += 1;
                self.cursor_row = self.height.saturating_sub(1);
                if !self.rows.is_empty() {
                    self.rows.remove(0);
                }
            } else {
                self.cursor_row += 1;
            }
        }

        fn put(&mut self, character: char) {
            let width = measure_text_width(&character.to_string()).max(1);
            if self.cursor_col + width > self.width {
                self.wraps += 1;
                self.newline();
            }
            self.ensure_row();
            self.rows[self.cursor_row].push(character);
            self.cursor_col += width;
        }

        fn ensure_row(&mut self) {
            while self.rows.len() <= self.cursor_row {
                self.rows.push(Vec::new());
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn interpret_screen(input: &str, height: usize, width: usize) -> Screen {
        let mut screen = Screen {
            rows: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            height,
            width,
            scrolls: 0,
            wraps: 0,
        };
        let chars: Vec<char> = input.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            match chars[index] {
                '\u{1b}' if chars.get(index + 1) == Some(&'[') => {
                    index += 2;
                    let start = index;
                    while index < chars.len()
                        && (chars[index].is_ascii_digit()
                            || chars[index] == ';'
                            || chars[index] == '?')
                    {
                        index += 1;
                    }
                    let params: String = chars[start..index].iter().collect();
                    let command = chars.get(index).copied().unwrap_or('\0');
                    if index < chars.len() {
                        index += 1;
                    }
                    screen.csi(&params, command);
                }
                '\r' => {
                    screen.cursor_col = 0;
                    index += 1;
                }
                '\n' => {
                    screen.newline();
                    index += 1;
                }
                character => {
                    screen.put(character);
                    index += 1;
                }
            }
        }
        screen
    }
}
