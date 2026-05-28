//! A small, self-contained vim-style modal editor for the review
//! popup's sentence edit mode (`Ctrl+E`). It is a *subset* of vim —
//! enough to fix a wrong correction quickly with the motions and
//! operators muscle memory expects — not a vim emulator.
//!
//! The state machine here is deliberately egui-free and pure so it can
//! be unit-tested by feeding [`VimKey`] sequences and asserting on the
//! resulting text/cursor/mode (the same testing style as
//! `hyprcorrect_core::buffer`). The popup ([`crate::review`]) owns the
//! egui rendering and translates egui key events into [`VimKey`]s.
//!
//! ## Supported (v1)
//! - Modes: NORMAL, INSERT, COMMAND (`:`).
//! - Enter INSERT: `i a I A o O`; leave with `Esc` (cursor steps left,
//!   vim-style).
//! - Motions (also operator targets): `h l w b e 0 ^ $ j k G`, `gg`,
//!   and the arrow keys.
//! - Edits: `x`, `r{char}`, `s`, `S`, `D`, `C`, `dd`, `cc`.
//! - Operator + motion / text object: `d{m}` and `c{m}` over
//!   `w b e h l 0 ^ $`; `diw ciw daw caw` (with the vim `cw`==`ce`
//!   special case).
//! - Leading counts: e.g. `3x`, `2w`, `2dw`.
//! - COMMAND: `:w :wq :x` (+ `!`/`a` variants) submit; `:q :q!` cancel;
//!   normal-mode `Enter` submits; INSERT `Enter` inserts a newline.
//!
//! ## Out of scope (v1)
//! Undo/redo, registers/yank/paste, visual mode, `.` repeat, marks,
//! search (`/`), and ex ranges — documented as non-goals in `DESIGN.md`.

/// A key handed to the editor. The popup maps egui events to these;
/// the machine never sees egui types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimKey {
    Char(char),
    Esc,
    Enter,
    Backspace,
    Left,
    Right,
    Up,
    Down,
}

/// What the popup should do after a key — most keys are [`None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimOutcome {
    /// Keep editing.
    None,
    /// Apply the (possibly edited) text and close — `:w`/`:wq`/`:x` or
    /// normal-mode `Enter`.
    Submit,
    /// Discard and close — `:q`/`:q!`.
    Cancel,
}

/// The current editing mode, for the status line / cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
}

/// A vim-subset modal editor over a single owned [`String`] (which may
/// grow multiple lines once the user inserts newlines).
#[derive(Debug)]
pub struct VimEdit {
    text: String,
    /// Byte offset into `text`; always a char boundary. In NORMAL mode
    /// it sits *on* a character (never on a `\n` or past the last char
    /// of a non-empty line); INSERT mode allows end-of-line.
    cursor: usize,
    mode: Mode,
    /// The `:`-command being typed (includes the leading `:`), valid
    /// only in [`Mode::Command`].
    cmdline: String,
    /// A transient status message (e.g. an unknown-command error),
    /// shown until the next NORMAL key.
    status: Option<String>,

    // --- pending NORMAL-mode parser state ---
    count: Option<usize>,
    /// A pending operator: `'d'` or `'c'`.
    op: Option<char>,
    /// `r` was pressed; the next char is the replacement.
    await_r: bool,
    /// `g` was pressed; expecting a second `g`.
    await_g: bool,
    /// After an operator, `i`/`a` was pressed; expecting `w`.
    await_textobj: Option<char>,
}

impl VimEdit {
    /// Create an editor over `text`, placing the NORMAL-mode cursor at
    /// (or just before) byte offset `cursor`.
    pub fn new(text: String, cursor: usize) -> Self {
        let mut v = Self {
            text,
            cursor: 0,
            mode: Mode::Normal,
            cmdline: String::new(),
            status: None,
            count: None,
            op: None,
            await_r: false,
            await_g: false,
            await_textobj: None,
        };
        v.cursor = cursor.min(v.text.len());
        while v.cursor > 0 && !v.text.is_char_boundary(v.cursor) {
            v.cursor -= 1;
        }
        v.clamp_normal();
        v
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The text for the bottom status line: `-- NORMAL --`,
    /// `-- INSERT --`, the `:`-command being typed, or a transient
    /// message.
    pub fn status_line(&self) -> String {
        match self.mode {
            Mode::Normal => self
                .status
                .clone()
                .unwrap_or_else(|| "-- NORMAL --".to_string()),
            Mode::Insert => "-- INSERT --".to_string(),
            Mode::Command => self.cmdline.clone(),
        }
    }

    /// Feed one key. Returns whether the popup should submit/cancel.
    pub fn handle(&mut self, key: VimKey) -> VimOutcome {
        match self.mode {
            Mode::Insert => {
                self.handle_insert(key);
                VimOutcome::None
            }
            Mode::Command => self.handle_command(key),
            Mode::Normal => self.handle_normal(key),
        }
    }

    // ----------------------------------------------------------------
    // NORMAL
    // ----------------------------------------------------------------

    fn handle_normal(&mut self, key: VimKey) -> VimOutcome {
        self.status = None;

        // Pending `r{char}`: the next key is the literal replacement.
        // Anything that isn't a printable char cancels it.
        if self.await_r {
            self.await_r = false;
            let n = self.count.take().unwrap_or(1);
            if let VimKey::Char(c) = key {
                self.replace_char(c, n);
            }
            return VimOutcome::None;
        }
        // Pending `g`: only a second `g` is meaningful.
        if self.await_g {
            self.await_g = false;
            if matches!(key, VimKey::Char('g')) {
                self.cursor = 0;
                self.clamp_normal();
            }
            self.reset_pending();
            return VimOutcome::None;
        }
        // Pending text object after an operator: expecting `w`.
        if let Some(ia) = self.await_textobj.take() {
            if matches!(key, VimKey::Char('w')) {
                let (s, e) = if ia == 'i' {
                    self.inner_word_bounds()
                } else {
                    self.a_word_bounds()
                };
                if let Some(op) = self.op.take() {
                    self.apply_op_range(op, s, e);
                }
            }
            self.reset_pending();
            return VimOutcome::None;
        }

        // Arrow keys are motions.
        let key = match key {
            VimKey::Left => VimKey::Char('h'),
            VimKey::Right => VimKey::Char('l'),
            VimKey::Up => VimKey::Char('k'),
            VimKey::Down => VimKey::Char('j'),
            other => other,
        };

        match key {
            VimKey::Esc => {
                self.reset_pending();
                VimOutcome::None
            }
            VimKey::Enter => VimOutcome::Submit,
            VimKey::Backspace => {
                self.move_motion('h');
                VimOutcome::None
            }
            VimKey::Char(c) => self.handle_normal_char(c),
            // Left/Right/Up/Down already remapped above.
            _ => VimOutcome::None,
        }
    }

    fn handle_normal_char(&mut self, c: char) -> VimOutcome {
        // Count digits. `0` is the column-0 motion when no count is in
        // progress, otherwise a digit.
        if c.is_ascii_digit() && !(c == '0' && self.count.is_none()) {
            let d = c as usize - '0' as usize;
            self.count = Some(self.count.unwrap_or(0) * 10 + d);
            return VimOutcome::None;
        }

        // Operator pending: this char is a doubling, text object, or
        // motion target.
        if let Some(op) = self.op {
            if c == op {
                // dd / cc — linewise.
                self.op = None;
                self.count = None;
                self.linewise_op(op);
                return VimOutcome::None;
            }
            if c == 'i' || c == 'a' {
                self.await_textobj = Some(c);
                return VimOutcome::None;
            }
            let n = self.count.take().unwrap_or(1);
            // vim's `cw` acts like `ce`: change to the end of the word,
            // not over the trailing whitespace.
            let motion = if op == 'c' && c == 'w' && self.class_at(self.cursor) != 0 {
                'e'
            } else {
                c
            };
            if let Some((s, e)) = self.op_span(motion, n) {
                self.op = None;
                self.apply_op_range(op, s, e);
            } else {
                self.reset_pending();
            }
            return VimOutcome::None;
        }

        match c {
            'd' | 'c' => self.op = Some(c),
            'i' => self.enter_insert_at(self.cursor),
            'a' => {
                let p = self
                    .next_boundary(self.cursor)
                    .min(self.line_end(self.cursor));
                self.enter_insert_at(p);
            }
            'I' => {
                let p = self.first_non_blank(self.cursor);
                self.enter_insert_at(p);
            }
            'A' => {
                let p = self.line_end(self.cursor);
                self.enter_insert_at(p);
            }
            'o' => {
                let le = self.line_end(self.cursor);
                self.text.insert(le, '\n');
                self.enter_insert_at(le + 1);
            }
            'O' => {
                let ls = self.line_start(self.cursor);
                self.text.insert(ls, '\n');
                self.enter_insert_at(ls);
            }
            'x' => {
                let n = self.count.take().unwrap_or(1);
                self.delete_chars(n);
            }
            's' => {
                let n = self.count.take().unwrap_or(1);
                self.delete_chars(n);
                self.mode = Mode::Insert;
            }
            'S' => self.linewise_op('c'),
            'D' => {
                let e = self.line_end(self.cursor);
                self.delete_range(self.cursor, e);
            }
            'C' => {
                let e = self.line_end(self.cursor);
                self.delete_range(self.cursor, e);
                self.mode = Mode::Insert;
            }
            'r' => self.await_r = true,
            'g' => self.await_g = true,
            ':' => {
                self.mode = Mode::Command;
                self.cmdline = String::from(":");
            }
            'h' | 'l' | 'w' | 'b' | 'e' | 'j' | 'k' | '0' | '^' | '$' | 'G' => self.move_motion(c),
            _ => self.count = None,
        }
        VimOutcome::None
    }

    fn reset_pending(&mut self) {
        self.count = None;
        self.op = None;
        self.await_r = false;
        self.await_g = false;
        self.await_textobj = None;
    }

    fn enter_insert_at(&mut self, pos: usize) {
        self.cursor = pos;
        self.mode = Mode::Insert;
        self.reset_pending();
    }

    fn move_motion(&mut self, c: char) {
        let n = self.count.take().unwrap_or(1);
        let mut p = self.cursor;
        match c {
            'h' => {
                for _ in 0..n {
                    p = self.step_left_on_line(p);
                }
            }
            'l' => {
                for _ in 0..n {
                    p = self.step_right_on_line(p);
                }
            }
            'w' => {
                for _ in 0..n {
                    p = self.next_word_start(p);
                }
            }
            'b' => {
                for _ in 0..n {
                    p = self.prev_word_start(p);
                }
            }
            'e' => {
                for _ in 0..n {
                    p = self.next_word_end(p);
                }
            }
            'j' => {
                for _ in 0..n {
                    p = self.line_down(p);
                }
            }
            'k' => {
                for _ in 0..n {
                    p = self.line_up(p);
                }
            }
            '0' => p = self.line_start(p),
            '^' => p = self.first_non_blank(p),
            '$' => p = self.line_last_char(p),
            'G' => p = self.last_line_start(),
            _ => {}
        }
        self.cursor = p;
        self.clamp_normal();
    }

    /// The byte range an operator should act over for `motion`,
    /// repeated `n` times, as `(start, end)` with `start <= end`.
    fn op_span(&self, motion: char, n: usize) -> Option<(usize, usize)> {
        let from = self.cursor;
        let mut p = from;
        let span = match motion {
            'w' => {
                for _ in 0..n {
                    p = self.next_word_start(p);
                }
                (from, p)
            }
            'e' => {
                for _ in 0..n {
                    p = self.next_word_end(p);
                }
                (from, self.next_boundary(p)) // `e` is inclusive
            }
            'b' => {
                for _ in 0..n {
                    p = self.prev_word_start(p);
                }
                (p, from)
            }
            'h' => {
                for _ in 0..n {
                    p = self.step_left_on_line(p);
                }
                (p, from)
            }
            'l' => {
                for _ in 0..n {
                    p = self.next_boundary_on_line(p);
                }
                (from, p)
            }
            '0' => (self.line_start(from), from),
            '^' => {
                let t = self.first_non_blank(from);
                (t.min(from), t.max(from))
            }
            '$' => (from, self.line_end(from)),
            _ => return None,
        };
        Some((span.0.min(span.1), span.0.max(span.1)))
    }

    fn apply_op_range(&mut self, op: char, start: usize, end: usize) {
        if start < end {
            self.text.replace_range(start..end, "");
            self.cursor = start;
        }
        if op == 'c' {
            self.mode = Mode::Insert;
        } else {
            self.clamp_normal();
        }
    }

    /// `dd` (delete line + its newline) / `cc`/`S` (clear line content,
    /// stay on it, enter insert).
    fn linewise_op(&mut self, op: char) {
        let ls = self.line_start(self.cursor);
        let le = self.line_end(self.cursor);
        if op == 'c' {
            self.text.replace_range(ls..le, "");
            self.cursor = ls;
            self.mode = Mode::Insert;
        } else {
            let end = if le < self.text.len() {
                self.next_boundary(le)
            } else {
                le
            };
            // Last line with no trailing newline: also drop the
            // preceding newline so the line truly disappears.
            let start = if end == le && ls > 0 {
                self.prev_boundary(ls)
            } else {
                ls
            };
            self.text.replace_range(start..end, "");
            self.cursor = self.line_start(start.min(self.text.len()));
            self.clamp_normal();
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let le = self.line_end(self.cursor);
        let mut e = self.cursor;
        for _ in 0..n {
            if e < le {
                e = self.next_boundary(e);
            }
        }
        self.delete_range(self.cursor, e);
    }

    fn delete_range(&mut self, start: usize, end: usize) {
        if start < end {
            self.text.replace_range(start..end, "");
            self.cursor = start.min(self.text.len());
            self.clamp_normal();
        }
    }

    fn replace_char(&mut self, c: char, n: usize) {
        let le = self.line_end(self.cursor);
        let mut end = self.cursor;
        for _ in 0..n {
            if end < le {
                end = self.next_boundary(end);
            }
        }
        if end == self.cursor {
            return;
        }
        let n_chars = self.text[self.cursor..end].chars().count();
        let repl: String = std::iter::repeat_n(c, n_chars).collect();
        let repl_len = repl.len();
        self.text.replace_range(self.cursor..end, &repl);
        // Land on the last replaced char (vim leaves the cursor there).
        self.cursor = self.cursor + repl_len - c.len_utf8();
        self.clamp_normal();
    }

    // ----------------------------------------------------------------
    // INSERT
    // ----------------------------------------------------------------

    fn handle_insert(&mut self, key: VimKey) {
        match key {
            VimKey::Esc => {
                self.mode = Mode::Normal;
                // vim steps the cursor left on leaving insert.
                let ls = self.line_start(self.cursor);
                if self.cursor > ls {
                    self.cursor = self.prev_boundary(self.cursor);
                }
                self.clamp_normal();
            }
            VimKey::Enter => {
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
            }
            VimKey::Backspace => {
                if self.cursor > 0 {
                    let pb = self.prev_boundary(self.cursor);
                    self.text.replace_range(pb..self.cursor, "");
                    self.cursor = pb;
                }
            }
            VimKey::Left => self.cursor = self.prev_boundary(self.cursor),
            VimKey::Right => self.cursor = self.next_boundary(self.cursor).min(self.text.len()),
            VimKey::Up => self.cursor = self.line_up(self.cursor),
            VimKey::Down => self.cursor = self.line_down(self.cursor),
            VimKey::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
        }
    }

    // ----------------------------------------------------------------
    // COMMAND (`:`)
    // ----------------------------------------------------------------

    fn handle_command(&mut self, key: VimKey) -> VimOutcome {
        match key {
            VimKey::Esc => {
                self.mode = Mode::Normal;
                self.cmdline.clear();
                VimOutcome::None
            }
            VimKey::Backspace => {
                self.cmdline.pop();
                if self.cmdline.is_empty() {
                    self.mode = Mode::Normal;
                }
                VimOutcome::None
            }
            VimKey::Enter => {
                let cmd = self.cmdline.trim_start_matches(':').trim().to_string();
                self.cmdline.clear();
                self.mode = Mode::Normal;
                match cmd.as_str() {
                    "w" | "wq" | "x" | "wq!" | "w!" | "x!" | "wqa" | "xa" => VimOutcome::Submit,
                    "q" | "q!" | "qa" | "qa!" => VimOutcome::Cancel,
                    other => {
                        self.status = Some(format!("E492: Not an editor command: {other}"));
                        VimOutcome::None
                    }
                }
            }
            VimKey::Char(c) => {
                self.cmdline.push(c);
                VimOutcome::None
            }
            _ => VimOutcome::None,
        }
    }

    // ----------------------------------------------------------------
    // cursor / line / word geometry (all return char boundaries)
    // ----------------------------------------------------------------

    /// Keep the NORMAL-mode cursor on a char boundary and never past
    /// the last character of its line.
    fn clamp_normal(&mut self) {
        while self.cursor > 0 && !self.text.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        let last = self.line_last_char(self.cursor);
        if self.cursor > last {
            self.cursor = last;
        }
    }

    fn prev_boundary(&self, pos: usize) -> usize {
        self.text[..pos]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, pos: usize) -> usize {
        self.text[pos..]
            .chars()
            .next()
            .map_or(pos, |c| pos + c.len_utf8())
    }

    fn line_start(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.text[pos..]
            .find('\n')
            .map_or(self.text.len(), |i| pos + i)
    }

    /// Byte offset of the last character on `pos`'s line, or the line
    /// start if the line is empty.
    fn line_last_char(&self, pos: usize) -> usize {
        let ls = self.line_start(pos);
        let le = self.line_end(pos);
        if le > ls { self.prev_boundary(le) } else { ls }
    }

    fn first_non_blank(&self, pos: usize) -> usize {
        let ls = self.line_start(pos);
        let le = self.line_end(pos);
        let mut i = ls;
        while i < le {
            let c = self.text[i..].chars().next().unwrap();
            if !c.is_whitespace() {
                return i;
            }
            i += c.len_utf8();
        }
        ls
    }

    fn step_left_on_line(&self, pos: usize) -> usize {
        let ls = self.line_start(pos);
        if pos > ls {
            self.prev_boundary(pos)
        } else {
            pos
        }
    }

    /// NORMAL-mode `l`: stops on the last char of the line.
    fn step_right_on_line(&self, pos: usize) -> usize {
        let last = self.line_last_char(pos);
        if pos < last {
            self.next_boundary(pos)
        } else {
            pos
        }
    }

    /// One char right, but not past end-of-line (used by `dl`, which
    /// must be able to reach just past the last char to delete it).
    fn next_boundary_on_line(&self, pos: usize) -> usize {
        self.next_boundary(pos).min(self.line_end(pos))
    }

    fn col(&self, pos: usize) -> usize {
        self.text[self.line_start(pos)..pos].chars().count()
    }

    fn place_col(&self, line_start: usize, col: usize) -> usize {
        let le = self.line_end(line_start);
        let mut i = line_start;
        let mut c = 0;
        while c < col && i < le {
            i = self.next_boundary(i);
            c += 1;
        }
        i
    }

    fn line_down(&self, pos: usize) -> usize {
        let le = self.line_end(pos);
        if le >= self.text.len() {
            return pos;
        }
        self.place_col(le + 1, self.col(pos))
    }

    fn line_up(&self, pos: usize) -> usize {
        let ls = self.line_start(pos);
        if ls == 0 {
            return pos;
        }
        let prev_ls = self.line_start(ls - 1);
        self.place_col(prev_ls, self.col(pos))
    }

    fn last_line_start(&self) -> usize {
        self.first_non_blank(self.text[..].rfind('\n').map_or(0, |i| i + 1))
    }

    /// Char class for word motions: 0 = whitespace, 1 = word
    /// (alphanumeric / `_` / `'`), 2 = other punctuation.
    fn class_at(&self, pos: usize) -> u8 {
        match self.text[pos..].chars().next() {
            None => 0,
            Some(c) if c.is_whitespace() => 0,
            Some(c) if c.is_alphanumeric() || c == '_' || c == '\'' => 1,
            Some(_) => 2,
        }
    }

    fn next_word_start(&self, pos: usize) -> usize {
        let len = self.text.len();
        if pos >= len {
            return len;
        }
        let mut i = pos;
        let cls = self.class_at(i);
        if cls != 0 {
            while i < len && self.class_at(i) == cls {
                i = self.next_boundary(i);
            }
        }
        while i < len && self.class_at(i) == 0 {
            i = self.next_boundary(i);
        }
        i
    }

    fn next_word_end(&self, pos: usize) -> usize {
        let len = self.text.len();
        if pos >= len {
            return pos;
        }
        let mut i = self.next_boundary(pos);
        while i < len && self.class_at(i) == 0 {
            i = self.next_boundary(i);
        }
        if i >= len {
            return self.prev_boundary(len);
        }
        let cls = self.class_at(i);
        loop {
            let nb = self.next_boundary(i);
            if nb < len && self.class_at(nb) == cls {
                i = nb;
            } else {
                break;
            }
        }
        i
    }

    fn prev_word_start(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let mut i = self.prev_boundary(pos);
        while i > 0 && self.class_at(i) == 0 {
            i = self.prev_boundary(i);
        }
        if self.class_at(i) == 0 {
            return i;
        }
        let cls = self.class_at(i);
        while i > 0 {
            let pb = self.prev_boundary(i);
            if self.class_at(pb) == cls {
                i = pb;
            } else {
                break;
            }
        }
        i
    }

    /// `iw`: the run of the class under the cursor.
    fn inner_word_bounds(&self) -> (usize, usize) {
        let pos = self.cursor;
        let len = self.text.len();
        if len == 0 {
            return (0, 0);
        }
        let cls = self.class_at(pos);
        let mut s = pos;
        while s > 0 {
            let pb = self.prev_boundary(s);
            if self.class_at(pb) == cls {
                s = pb;
            } else {
                break;
            }
        }
        let mut e = pos;
        while e < len && self.class_at(e) == cls {
            e = self.next_boundary(e);
        }
        (s, e)
    }

    /// `aw`: the word plus its trailing whitespace, or leading
    /// whitespace if there is no trailing.
    fn a_word_bounds(&self) -> (usize, usize) {
        let (s, mut e) = self.inner_word_bounds();
        let len = self.text.len();
        if e < len && self.class_at(e) == 0 {
            while e < len && self.class_at(e) == 0 {
                e = self.next_boundary(e);
            }
            return (s, e);
        }
        let mut s2 = s;
        while s2 > 0 {
            let pb = self.prev_boundary(s2);
            if self.class_at(pb) == 0 {
                s2 = pb;
            } else {
                break;
            }
        }
        (s2, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed each char of `s` as a NORMAL/INSERT key press.
    fn feed(v: &mut VimEdit, s: &str) -> VimOutcome {
        let mut out = VimOutcome::None;
        for c in s.chars() {
            out = v.handle(VimKey::Char(c));
        }
        out
    }

    #[test]
    fn x_deletes_char_under_cursor() {
        let mut v = VimEdit::new("abc".into(), 0);
        feed(&mut v, "x");
        assert_eq!(v.text(), "bc");
        assert_eq!(v.cursor(), 0);
    }

    #[test]
    fn count_x_deletes_n_chars() {
        let mut v = VimEdit::new("abcdef".into(), 0);
        feed(&mut v, "3x");
        assert_eq!(v.text(), "def");
    }

    #[test]
    fn dw_deletes_word_and_trailing_space() {
        let mut v = VimEdit::new("the quick".into(), 0);
        feed(&mut v, "dw");
        assert_eq!(v.text(), "quick");
    }

    #[test]
    fn diw_deletes_inner_word_keeping_space() {
        let mut v = VimEdit::new("the quick".into(), 0);
        feed(&mut v, "diw");
        assert_eq!(v.text(), " quick");
    }

    #[test]
    fn ciw_then_type_replaces_the_word() {
        let mut v = VimEdit::new("teh quick".into(), 0);
        feed(&mut v, "ciw");
        assert_eq!(v.mode(), Mode::Insert);
        assert_eq!(v.text(), " quick");
        feed(&mut v, "the");
        assert_eq!(v.text(), "the quick");
        assert_eq!(v.cursor(), 3);
    }

    #[test]
    fn cw_behaves_like_ce_changing_to_end_of_word() {
        let mut v = VimEdit::new("teh quick".into(), 0);
        feed(&mut v, "cw");
        // Only "teh" is removed, the space stays (cw == ce).
        assert_eq!(v.text(), " quick");
        assert_eq!(v.mode(), Mode::Insert);
        feed(&mut v, "the");
        assert_eq!(v.text(), "the quick");
    }

    #[test]
    fn daw_deletes_word_with_its_space() {
        let mut v = VimEdit::new("the quick fox".into(), 4); // on "quick"
        feed(&mut v, "daw");
        assert_eq!(v.text(), "the fox");
    }

    #[test]
    fn replace_char_with_r() {
        let mut v = VimEdit::new("cat".into(), 0);
        feed(&mut v, "rb");
        assert_eq!(v.text(), "bat");
        assert_eq!(v.cursor(), 0);
    }

    #[test]
    fn word_motions_move_the_cursor() {
        let mut v = VimEdit::new("the quick brown".into(), 0);
        feed(&mut v, "w");
        assert_eq!(v.cursor(), 4); // start of "quick"
        feed(&mut v, "e");
        assert_eq!(v.cursor(), 8); // end of "quick" ('k')
        feed(&mut v, "b");
        assert_eq!(v.cursor(), 4); // back to start of "quick"
    }

    #[test]
    fn count_word_motion() {
        let mut v = VimEdit::new("one two three four".into(), 0);
        feed(&mut v, "2w");
        assert_eq!(v.cursor(), 8); // start of "three"
    }

    #[test]
    fn dollar_and_zero_jump_to_line_edges() {
        let mut v = VimEdit::new("hello world".into(), 0);
        feed(&mut v, "$");
        assert_eq!(v.cursor(), 10); // 'd', last char
        feed(&mut v, "0");
        assert_eq!(v.cursor(), 0);
    }

    #[test]
    fn append_inserts_after_cursor() {
        let mut v = VimEdit::new("ab".into(), 0);
        feed(&mut v, "a"); // append after 'a'
        assert_eq!(v.mode(), Mode::Insert);
        feed(&mut v, "X");
        assert_eq!(v.text(), "aXb");
    }

    #[test]
    fn capital_a_appends_at_end_of_line() {
        let mut v = VimEdit::new("abc".into(), 0);
        feed(&mut v, "A");
        feed(&mut v, "Z");
        assert_eq!(v.text(), "abcZ");
    }

    #[test]
    fn esc_leaves_insert_and_steps_left() {
        let mut v = VimEdit::new("abc".into(), 0);
        feed(&mut v, "A"); // insert at end, cursor == 3
        v.handle(VimKey::Esc);
        assert_eq!(v.mode(), Mode::Normal);
        assert_eq!(v.cursor(), 2); // stepped back onto 'c'
    }

    #[test]
    fn dd_on_single_line_empties_it() {
        let mut v = VimEdit::new("hello".into(), 0);
        feed(&mut v, "dd");
        assert_eq!(v.text(), "");
    }

    #[test]
    fn capital_d_deletes_to_end_of_line() {
        let mut v = VimEdit::new("hello world".into(), 6); // on "world"
        feed(&mut v, "D");
        assert_eq!(v.text(), "hello ");
    }

    #[test]
    fn insert_enter_adds_a_newline() {
        let mut v = VimEdit::new("ab".into(), 0);
        feed(&mut v, "A"); // insert at end
        v.handle(VimKey::Enter);
        feed(&mut v, "c");
        assert_eq!(v.text(), "ab\nc");
    }

    #[test]
    fn normal_enter_submits() {
        let mut v = VimEdit::new("hello".into(), 0);
        assert_eq!(v.handle(VimKey::Enter), VimOutcome::Submit);
    }

    #[test]
    fn colon_wq_submits() {
        let mut v = VimEdit::new("hello".into(), 0);
        assert_eq!(feed(&mut v, ":wq"), VimOutcome::None);
        assert_eq!(v.handle(VimKey::Enter), VimOutcome::Submit);
    }

    #[test]
    fn colon_w_submits() {
        let mut v = VimEdit::new("hello".into(), 0);
        feed(&mut v, ":w");
        assert_eq!(v.handle(VimKey::Enter), VimOutcome::Submit);
    }

    #[test]
    fn colon_q_cancels() {
        let mut v = VimEdit::new("hello".into(), 0);
        feed(&mut v, ":q");
        assert_eq!(v.handle(VimKey::Enter), VimOutcome::Cancel);
    }

    #[test]
    fn unknown_command_reports_and_stays_open() {
        let mut v = VimEdit::new("hello".into(), 0);
        feed(&mut v, ":frobnicate");
        assert_eq!(v.handle(VimKey::Enter), VimOutcome::None);
        assert_eq!(v.mode(), Mode::Normal);
        assert!(v.status_line().contains("Not an editor command"));
    }

    #[test]
    fn command_backspace_past_colon_returns_to_normal() {
        let mut v = VimEdit::new("hi".into(), 0);
        feed(&mut v, ":w");
        v.handle(VimKey::Backspace);
        v.handle(VimKey::Backspace); // removes the ':'
        assert_eq!(v.mode(), Mode::Normal);
    }

    #[test]
    fn full_fix_flow_ciw_replace_then_wq() {
        // The headline use case: correction was wrong, fix one word.
        let mut v = VimEdit::new("the quick browne fox".into(), 10); // on "browne"
        feed(&mut v, "ciw");
        feed(&mut v, "brown");
        assert_eq!(v.text(), "the quick brown fox");
        v.handle(VimKey::Esc); // back to NORMAL before the command
        assert_eq!(feed(&mut v, ":wq"), VimOutcome::None);
        assert_eq!(v.handle(VimKey::Enter), VimOutcome::Submit);
    }

    #[test]
    fn multibyte_word_edit() {
        let mut v = VimEdit::new("cafe au lait".into(), 0);
        feed(&mut v, "cw"); // change "cafe"
        feed(&mut v, "café");
        assert_eq!(v.text(), "café au lait");
    }

    #[test]
    fn open_line_below_enters_insert_on_new_line() {
        let mut v = VimEdit::new("abc".into(), 0);
        feed(&mut v, "o");
        assert_eq!(v.mode(), Mode::Insert);
        feed(&mut v, "xy");
        assert_eq!(v.text(), "abc\nxy");
    }

    #[test]
    fn j_k_move_between_lines() {
        let mut v = VimEdit::new("abc\ndef".into(), 1); // on 'b'
        feed(&mut v, "j");
        assert_eq!(v.cursor(), 5); // 'e' on line 2, same column
        feed(&mut v, "k");
        assert_eq!(v.cursor(), 1); // back to 'b'
    }
}
