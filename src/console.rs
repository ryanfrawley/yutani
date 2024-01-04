use super::ring_buffer;
use std::iter::Skip;

pub struct Console {
    pub buffer: ring_buffer::RingBuffer<char>,
    pub columns: usize,
    pub rows: usize,
    pub scroll_ptr: usize,
    pub scroll_y: usize,
    pub cursor_offset: usize,
    pub input: String,
}

impl Console {
    pub fn new(columns: usize, rows: usize, buffer_len: usize, input_len: usize) -> Self {
        if buffer_len == 0 {
            panic!("buffer_len must be > 0!");
        }

        Self {
            buffer: ring_buffer::RingBuffer::new(buffer_len),
            input: String::with_capacity(input_len),
            rows,
            columns,
            scroll_y: 0,
            scroll_ptr: 0,
            cursor_offset: 0,
        }
    }

    pub fn resize(&mut self, columns: usize, rows: usize) {
        self.columns = columns;
        self.rows = rows;
    }

    pub fn write(&mut self, str: &str) {
        for c in str.chars() {
            self.buffer.push_back(c);
        }
    }


    pub fn write_input(&mut self) {
        for c in self.input.chars() {
            self.buffer.push_back(c);
        }
        self.buffer.push_back('\n');
        self.cursor_offset = 0;
        self.input.clear();
    }

    pub fn delete_left(&mut self) -> bool {
        if self.cursor_offset == 0 {
            return false;
        }
        self.input.remove(self.cursor_offset - 1);
        self.cursor_offset = match self.cursor_offset {
            0 => 0,
            _ => self.cursor_offset - 1,
        };
        true
    }

    pub fn insert_right(&mut self, c: char) -> bool {
        self.input.insert(self.cursor_offset, c);
        self.cursor_offset += 1;
        true
    }

    pub fn iter(&self) -> ring_buffer::RingBufferIterator<char> {
        self.buffer.iter()
    }

    pub fn iter_view(&self) -> Skip<ring_buffer::RingBufferIterator<char>> {
        self.buffer.iter().skip(self.scroll_ptr)
    }

    pub fn cursor_left(&mut self) -> bool {
        if self.cursor_offset == 0 {
            return false;
        }
        self.cursor_offset -= 1;
        true
    }

    pub fn cursor_right(&mut self) -> bool {
        if self.cursor_offset >= self.input.len() {
            return false;
        }
        self.cursor_offset += 1;
        true
    }

    pub fn scroll_down(&mut self) -> bool {
        if self.scroll_ptr >= self.buffer.len() {
            return false;
        }

        let mut col = 0;
        while col != self.columns {
            col += 1;
            if self.buffer[self.scroll_ptr + col - 1] == '\n' {
                break;
            }
        }
        self.scroll_ptr += col;
        self.scroll_y += 1;
        true
    }

    pub fn scroll_up(&mut self) -> bool {
        if self.scroll_y == 0 {
            return false;
        }

        let mut count = 0;
        while count != self.columns && self.scroll_ptr - count > 0 {
            if count > 0 && self.buffer[self.scroll_ptr - count - 1] == '\n' {
                break;
            }
            count += 1;
        }
        self.scroll_ptr = self.scroll_ptr - count;
        self.scroll_y -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new() {
        let console = Console::new(5, 3, 64, 1024);
        assert_eq!(console.columns, 5);
        assert_eq!(console.rows, 3);
        assert_eq!(console.buffer.len(), 0);
        assert_eq!(console.input.len(), 0);
    }

    #[test]
    fn write1() {
        let mut console = Console::new(5, 3, 64, 1024);
        console.write("Hello");
        assert_eq!(console.buffer[0], 'H');
        assert_eq!(console.buffer[1], 'e');
        assert_eq!(console.buffer[2], 'l');
        assert_eq!(console.buffer[3], 'l');
        assert_eq!(console.buffer[4], 'o');
    }

    #[test]
    fn write2() {
        let mut console = Console::new(5, 3, 64, 1024);
        console.write("Hello");
        console.write(" ");
        console.write("world");
        assert_eq!(console.buffer[0], 'H');
        assert_eq!(console.buffer[1], 'e');
        assert_eq!(console.buffer[2], 'l');
        assert_eq!(console.buffer[3], 'l');
        assert_eq!(console.buffer[4], 'o');
        assert_eq!(console.buffer[5], ' ');
        assert_eq!(console.buffer[6], 'w');
        assert_eq!(console.buffer[7], 'o');
        assert_eq!(console.buffer[8], 'r');
        assert_eq!(console.buffer[9], 'l');
        assert_eq!(console.buffer[10], 'd');
    }


    #[test]
    fn iter_view1() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "This should take up multiple lines.";
        let chars: Vec<char> = text.chars().collect();
        console.write(text);
        let mut count = 0;
        for (idx, c) in console.iter_view().enumerate() {
            count += 1;
            assert_eq!(*c, chars[idx]);
        }
        assert_eq!(count, chars.len());
    }

    #[test]
    fn scroll_down1() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorld";
        console.write(text);
        assert_eq!(console.scroll_down(), true);
        assert_eq!(console.scroll_y, 1);
        assert_eq!(console.scroll_ptr, 5);
        assert_eq!(console.buffer[console.scroll_ptr], 'W');
    }

    #[test]
    fn scroll_down2() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldTest";
        console.write(text);
        assert_eq!(console.scroll_down(), true);
        assert_eq!(console.scroll_down(), true);
        assert_eq!(console.scroll_y, 2);
        assert_eq!(console.scroll_ptr, 10);
        assert_eq!(console.buffer[console.scroll_ptr], 'T');
    }

    #[test]
    fn scroll_down3() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldZ";
        console.write(text);
        assert_eq!(console.scroll_down(), true);
        assert_eq!(console.scroll_down(), true);
        assert_eq!(console.scroll_y, 2);
        assert_eq!(console.scroll_ptr, 10);
        assert_eq!(console.buffer[console.scroll_ptr], 'Z');
    }

    #[test]
    fn scroll_up() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldStars";
        console.write(text);
        console.scroll_y = 1;
        console.scroll_ptr = 5;
        assert_eq!(console.scroll_up(), true);
        assert_eq!(console.scroll_y, 0);
        assert_eq!(console.scroll_ptr, 0);
    }

    #[test]
    fn scroll_up_line_break() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "Hi\nFren";
        console.write(text);
        console.scroll_y = 1;
        console.scroll_ptr = 3;
        assert_eq!(console.scroll_up(), true);
        assert_eq!(console.scroll_y, 0);
        assert_eq!(console.scroll_ptr, 0);
    }

    #[test]
    fn scroll_down_invalid() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldStars";
        console.write(text);
        assert_eq!(console.scroll_up(), false);
        assert_eq!(console.scroll_y, 0);
        assert_eq!(console.scroll_ptr, 0);
    }

    #[test]
    fn iter_scroll() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "This should take up multiple lines.";
        let chars: Vec<char> = text.chars().collect();
        console.write(text);
        console.scroll_down();
        let mut count = 0;
        for (idx, c) in console.iter_view().enumerate() {
            assert_eq!(chars[idx + 5], *c);
            count += 1;
        }
        assert_eq!(count, chars.len() - 5);
        count = 0;
        console.scroll_up();
        for (idx, c) in console.iter_view().enumerate() {
            assert_eq!(chars[idx], *c);
            count += 1;
        }
        assert_eq!(count, chars.len());
    }

    // TODO: Figure out why scrolling up can break if we have a full width output line with no line
    // breaks.
}
