use super::ring_buffer;

pub struct Console {
    pub buffer: ring_buffer::RingBuffer<char>,
    pub columns: usize,
    pub rows: usize,
    pub scroll_y: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub input: Vec<char>,
}

impl Console {
    pub fn new(columns: usize, rows: usize, buffer_len: usize, input_len: usize) -> Self {
        if buffer_len == 0 {
            panic!("buffer_len must be > 0!");
        }

        Self {
            buffer: ring_buffer::RingBuffer::new(buffer_len),
            input: Vec::with_capacity(input_len),
            rows,
            columns,
            scroll_y: 0,
            cursor_x: 0,
            cursor_y: 0,
        }
    }

    pub fn write(&mut self, str: &str) {
        let len = self.buffer.len();
        for c in str.chars() {
            self.buffer.push_back(c);
        }
    }

    pub fn iter(&self) -> ring_buffer::RingBufferIterator<char> {
        self.buffer.iter()
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
}
