/// Local-echo input widget state. This is the user's pre-submit edit buffer,
/// separate from the terminal model — nothing here has been sent to the PTY yet.
pub struct InputState {
    pub text: String,
    pub cursor_offset: usize,
}

impl InputState {
    pub fn new(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            cursor_offset: 0,
        }
    }

    pub fn insert_right(&mut self, c: char) -> bool {
        self.text.insert(self.cursor_offset, c);
        self.cursor_offset += 1;
        true
    }

    pub fn delete_left(&mut self) -> bool {
        if self.cursor_offset == 0 {
            return false;
        }
        self.text.remove(self.cursor_offset - 1);
        self.cursor_offset -= 1;
        true
    }

    pub fn cursor_left(&mut self) -> bool {
        if self.cursor_offset == 0 {
            return false;
        }
        self.cursor_offset -= 1;
        true
    }

    pub fn cursor_right(&mut self) -> bool {
        if self.cursor_offset >= self.text.len() {
            return false;
        }
        self.cursor_offset += 1;
        true
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_advances_cursor() {
        let mut s = InputState::new(16);
        s.insert_right('a');
        s.insert_right('b');
        assert_eq!(s.text, "ab");
        assert_eq!(s.cursor_offset, 2);
    }

    #[test]
    fn delete_left_at_start_is_noop() {
        let mut s = InputState::new(16);
        s.insert_right('a');
        s.cursor_offset = 0;
        assert!(!s.delete_left());
        assert_eq!(s.text, "a");
    }

    #[test]
    fn cursor_moves_clamp_to_bounds() {
        let mut s = InputState::new(16);
        s.insert_right('a');
        assert!(!s.cursor_right());
        assert!(s.cursor_left());
        assert!(!s.cursor_left());
    }

    #[test]
    fn clear_resets_everything() {
        let mut s = InputState::new(16);
        s.insert_right('a');
        s.insert_right('b');
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.cursor_offset, 0);
    }

    #[test]
    fn insert_in_middle() {
        let mut s = InputState::new(16);
        s.insert_right('a');
        s.insert_right('c');
        s.cursor_left();
        s.insert_right('b');
        assert_eq!(s.text, "abc");
        assert_eq!(s.cursor_offset, 2);
    }
}
