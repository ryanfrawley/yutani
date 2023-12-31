use super::ring_buffer;
use std::iter::Skip;
use std::collections::VecDeque;

pub struct Console {
    pub buffer: ring_buffer::RingBuffer<char>,
    pub columns: usize,
    pub rows: usize,
    pub scroll_ptr: usize,
    pub scroll_y: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
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
            cursor_x: 0,
            cursor_y: 0,
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
        self.input.clear();
    }

    pub fn iter(&self) -> ring_buffer::RingBufferIterator<char> {
        self.buffer.iter()
    }

    pub fn iter_view(&self) -> Skip<ring_buffer::RingBufferIterator<char>> {
        self.buffer.iter().skip(self.scroll_ptr)
    }

    pub fn scroll_by(&mut self, y: isize) -> bool {
        let mut scrolled = 0;
        let scroll_target = isize::abs(y);
        let mut column = 0;
        let mut idx = self.scroll_ptr;
        let mut ptr = self.scroll_ptr;
        while scrolled != scroll_target {
            if idx == self.buffer.len() {
                return false;
            }

            if self.buffer[idx] == '\n' || column == self.columns {
                scrolled += 1;
                ptr = idx;
                column = 0;
            }

            if scrolled == scroll_target {
                break;
            }

            if idx == 0 && y < 0 {
                return false;
            }

            column += 1;

            idx = match y {
                _ if y > 0 => idx + 1,
                _ => idx - 1,
            };
        }
        self.scroll_ptr = ptr;
        self.scroll_y = (self.scroll_y as isize + y) as usize;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let console = Console::new(5, 3, 64, 1024);
        assert_eq!(console.columns, 5);
        assert_eq!(console.rows, 3);
        assert_eq!(console.buffer.len(), 0);
        assert_eq!(console.input.len(), 0);
    }

    #[test]
    fn test_write1() {
        let mut console = Console::new(5, 3, 64, 1024);
        console.write("Hello");
        assert_eq!(console.buffer[0], 'H');
        assert_eq!(console.buffer[1], 'e');
        assert_eq!(console.buffer[2], 'l');
        assert_eq!(console.buffer[3], 'l');
        assert_eq!(console.buffer[4], 'o');
    }

    #[test]
    fn test_write2() {
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
    fn test_iter_view1() {
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
    fn test_scroll_up1() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorld";
        console.write(text);
        assert_eq!(console.scroll_by(1), true);
        assert_eq!(console.scroll_y, 1);
        assert_eq!(console.scroll_ptr, 5);
        assert_eq!(console.buffer[console.scroll_ptr], 'W');
    }

    #[test]
    fn test_scroll_up2() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldTest";
        console.write(text);
        assert_eq!(console.scroll_by(1), true);
        assert_eq!(console.scroll_by(1), true);
        assert_eq!(console.scroll_y, 2);
        assert_eq!(console.scroll_ptr, 10);
        assert_eq!(console.buffer[console.scroll_ptr], 'T');
    }

    #[test]
    fn test_scroll_up3() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldZ";
        console.write(text);
        assert_eq!(console.scroll_by(2), true);
        assert_eq!(console.scroll_y, 2);
        assert_eq!(console.scroll_ptr, 10);
        assert_eq!(console.buffer[console.scroll_ptr], 'Z');
    }

    #[test]
    fn test_scroll_none() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldTest";
        console.write(text);
        assert_eq!(console.scroll_by(0), true);
        assert_eq!(console.scroll_y, 0);
        assert_eq!(console.scroll_ptr, 0);
        assert_eq!(console.buffer[console.scroll_ptr], 'H');
    }

    #[test]
    fn test_scroll_down() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldStars";
        console.write(text);
        console.scroll_y = 1;
        console.scroll_ptr = 5;
        assert_eq!(console.scroll_by(-1), true);
        assert_eq!(console.scroll_y, 0);
        assert_eq!(console.scroll_ptr, 0);
    }

    #[test]
    fn test_scroll_down_invalid() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "HelloWorldStars";
        console.write(text);
        assert_eq!(console.scroll_by(-1), false);
        assert_eq!(console.scroll_y, 0);
        assert_eq!(console.scroll_ptr, 0);
    }

    #[test]
    fn test_iter_scroll() {
        let mut console = Console::new(5, 3, 64, 1024);
        let text = "This should take up multiple lines.";
        let chars: Vec<char> = text.chars().collect();
        console.write(text);
        for (idx, c) in console.iter_view().enumerate() {
            assert_eq!(chars[idx], *c);
        }
    }
}
