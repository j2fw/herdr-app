//! Readline's edits of a one-line text and its cursor: the terminal find
//! bar's Ctrl+W, Ctrl+U, Alt+B and the rest, which its text input lacks.

/// The text, the cursor (a byte offset on a character boundary), and whether
/// all of it is selected (what typing replaces).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineEdit {
    pub text: String,
    pub cursor: usize,
    pub all: bool,
}

impl LineEdit {
    #[cfg(test)]
    /// The cursor at the end.
    pub fn new(text: &str) -> Self {
        LineEdit { text: text.to_owned(), cursor: text.len(), all: false }
    }

    #[cfg(test)]
    /// All of it selected, so typing replaces it.
    pub fn selected(text: &str) -> Self {
        LineEdit { all: !text.is_empty(), ..Self::new(text) }
    }

    #[cfg(test)]
    /// The text before and after the cursor.
    pub fn split(&self) -> (&str, &str) {
        self.text.split_at(self.cursor)
    }

    /// A selection goes first: edits remove it, moves drop it.
    fn take_all(&mut self) -> bool {
        if std::mem::take(&mut self.all) {
            self.text.clear();
            self.cursor = 0;
            return true;
        }
        false
    }

    fn deselect_to(&mut self, end: bool) -> bool {
        if std::mem::take(&mut self.all) {
            self.cursor = if end { self.text.len() } else { 0 };
            return true;
        }
        false
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
    }

    #[cfg(test)]
    pub fn insert(&mut self, s: &str) {
        self.take_all();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Backspace, Ctrl+H.
    pub fn backspace(&mut self) {
        if !self.take_all() && self.cursor > 0 {
            let p = self.prev(self.cursor);
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
    }

    /// Delete, Ctrl+D.
    pub fn delete(&mut self) {
        if !self.take_all() {
            let n = self.next(self.cursor);
            self.text.replace_range(self.cursor..n, "");
        }
    }

    /// Left, Ctrl+B.
    pub fn left(&mut self) {
        if !self.deselect_to(false) {
            self.cursor = self.prev(self.cursor);
        }
    }

    /// Right, Ctrl+F.
    pub fn right(&mut self) {
        if !self.deselect_to(true) {
            self.cursor = self.next(self.cursor);
        }
    }

    /// Home, Ctrl+A.
    pub fn home(&mut self) {
        self.all = false;
        self.cursor = 0;
    }

    /// End, Ctrl+E.
    pub fn end(&mut self) {
        self.all = false;
        self.cursor = self.text.len();
    }

    /// Where Alt+B goes: back over non-word characters, then the word.
    fn word_start(&self) -> usize {
        let mut at = self.cursor;
        while at > 0 && !is_word(self.text[..at].chars().next_back()) {
            at = self.prev(at);
        }
        while at > 0 && is_word(self.text[..at].chars().next_back()) {
            at = self.prev(at);
        }
        at
    }

    /// Where Alt+F goes: over non-word characters, then to the word's end.
    fn word_end(&self) -> usize {
        let mut at = self.cursor;
        while at < self.text.len() && !is_word(self.text[at..].chars().next()) {
            at = self.next(at);
        }
        while at < self.text.len() && is_word(self.text[at..].chars().next()) {
            at = self.next(at);
        }
        at
    }

    /// Alt+B.
    pub fn word_left(&mut self) {
        if !self.deselect_to(false) {
            self.cursor = self.word_start();
        }
    }

    /// Alt+F.
    pub fn word_right(&mut self) {
        if !self.deselect_to(true) {
            self.cursor = self.word_end();
        }
    }

    /// Ctrl+W: back to the previous blank (readline's unix-word-rubout).
    pub fn rubout(&mut self) {
        if self.take_all() {
            return;
        }
        let mut at = self.cursor;
        while at > 0 && self.text[..at].ends_with(char::is_whitespace) {
            at = self.prev(at);
        }
        while at > 0 && !self.text[..at].ends_with(char::is_whitespace) {
            at = self.prev(at);
        }
        self.text.replace_range(at..self.cursor, "");
        self.cursor = at;
    }

    /// Alt+Backspace: back to the start of the word.
    pub fn kill_word_back(&mut self) {
        if !self.take_all() {
            let at = self.word_start();
            self.text.replace_range(at..self.cursor, "");
            self.cursor = at;
        }
    }

    /// Alt+D: to the end of the word.
    pub fn kill_word(&mut self) {
        if !self.take_all() {
            let at = self.word_end();
            self.text.replace_range(self.cursor..at, "");
        }
    }

    /// Ctrl+U.
    pub fn kill_to_start(&mut self) {
        if !self.take_all() {
            self.text.replace_range(..self.cursor, "");
            self.cursor = 0;
        }
    }

    /// Ctrl+K.
    pub fn kill_to_end(&mut self) {
        if !self.take_all() {
            self.text.truncate(self.cursor);
        }
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::LineEdit;

    fn at(text: &str, cursor: usize) -> LineEdit {
        LineEdit { text: text.into(), cursor, all: false }
    }

    #[test]
    fn a_selected_query_is_replaced_by_typing_and_kept_by_moving() {
        let mut e = LineEdit::selected("old query");
        e.insert("n");
        assert_eq!(e, at("n", 1));
        let mut e = LineEdit::selected("old query");
        e.backspace();
        assert_eq!(e, at("", 0));
        let mut e = LineEdit::selected("old query");
        e.left();
        assert_eq!(e, at("old query", 0));
        let mut e = LineEdit::selected("old query");
        e.end();
        e.insert("!");
        assert_eq!(e, at("old query!", 10));
        assert_eq!(LineEdit::selected(""), at("", 0), "nothing to select");
    }

    #[test]
    fn readline_moves_and_kills() {
        let mut e = LineEdit::new("cargo test --bin herdr");
        e.home();
        e.word_right();
        assert_eq!(e.cursor, 5, "the end of cargo");
        e.word_right();
        assert_eq!(e.cursor, 10, "the end of test");
        e.word_right();
        assert_eq!(e.cursor, 16, "past -- to the end of bin");
        e.word_left();
        assert_eq!(e.cursor, 13, "the start of bin");
        e.kill_word();
        assert_eq!(e, at("cargo test -- herdr", 13));
        e.kill_word_back();
        assert_eq!(e, at("cargo  herdr", 6), "back over -- and the blank, then test");
    }

    #[test]
    fn readline_kills() {
        let mut e = LineEdit::new("foo.bar baz");
        e.rubout();
        assert_eq!(e, at("foo.bar ", 8), "Ctrl+W: back to the blank");
        e.rubout();
        assert_eq!(e, at("", 0), "over the blank, then foo.bar");
        let mut e = LineEdit::new("foo.bar baz");
        e.kill_word_back();
        assert_eq!(e, at("foo.bar ", 8));
        e.kill_word_back();
        assert_eq!(e, at("foo.", 4), "Alt+Backspace: words, not blanks");
        let mut e = at("hello world", 5);
        e.kill_to_end();
        assert_eq!(e, at("hello", 5));
        let mut e = at("hello world", 6);
        e.kill_to_start();
        assert_eq!(e, at("world", 0));
        let mut e = at("héllo", 1);
        e.delete();
        assert_eq!(e, at("hllo", 1), "é is one character");
    }

    #[test]
    fn moves_by_characters() {
        let mut e = at("añb", 1);
        e.right();
        assert_eq!(e.cursor, 3, "ñ is two bytes");
        e.backspace();
        assert_eq!(e, at("ab", 1));
        e.left();
        e.left();
        assert_eq!(e.cursor, 0);
        e.insert("x");
        assert_eq!(e, at("xab", 1));
        assert_eq!(e.split(), ("x", "ab"));
    }
}
