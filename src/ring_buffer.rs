use std::ops::Index;

#[derive(Debug)]
pub struct RingBuffer<T> {
    head: usize,
    buffer: Vec<T>,
}

impl<T> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            head: 0,
            buffer: Vec::with_capacity(capacity),
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn iter(&self) -> RingBufferIterator<T> {
        let len = self.buffer.len();
        RingBufferIterator {
            next: self.head,
            end: (self.head + len) % len,
            buffer: self,
        }
    }

    pub fn push_back(&mut self, item: T) {
        if self.buffer.len() < self.buffer.capacity() {
            self.buffer.push(item);
        } else {
            self.buffer[self.head] = item;
            self.head += 1;
            if self.head == self.buffer.len() {
                self.head = 0;
            }
        }
    }
}

pub struct RingBufferIterator<'a, T> {
    buffer: &'a RingBuffer<T>,
    end: usize,
    next: usize,
}

impl<'a, T> Iterator for RingBufferIterator<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.end {
            return None;
        }
        let result = &self.buffer.buffer[self.next];
        self.next += 1;
        if self.next == self.buffer.buffer.len() {
            self.next = 0;
        }
        Some(result)
    }
}

impl<T> Index<usize> for RingBuffer<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        if index >= self.buffer.len() {
            panic!("index {} out of buffer range {}", index, self.buffer.len());
        }
        &self.buffer[(self.head + index) % self.buffer.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty() {
        let buffer: RingBuffer<u8> = RingBuffer::new(5);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn test_push1() {
        let mut buffer: RingBuffer<u8> = RingBuffer::new(5);
        buffer.push_back(99);
        assert_eq!(buffer.buffer, [99]);
    }

    #[test]
    fn test_overflow() {
        let mut buffer: RingBuffer<u8> = RingBuffer::new(3);
        buffer.push_back(1);
        buffer.push_back(2);
        buffer.push_back(3);
        buffer.push_back(4);
        assert_eq!(buffer.buffer, [4, 2, 3]);
    }

    #[test]
    fn test_iter() {
        let mut buffer: RingBuffer<u8> = RingBuffer::new(3);
        buffer.push_back(1);
        buffer.push_back(2);
        buffer.push_back(3);
        for (idx, i) in buffer.iter().enumerate() {
            match idx {
                0 => assert_eq!(*i, 1),
                1 => assert_eq!(*i, 2),
                2 => assert_eq!(*i, 3),
                _ => assert!(false)
            }
        }
    }

    #[test]
    fn test_iter_underflow() {
        let mut buffer: RingBuffer<u8> = RingBuffer::new(10);
        buffer.push_back(1);
        buffer.push_back(2);
        buffer.push_back(3);
        for (idx, i) in buffer.iter().enumerate() {
            match idx {
                0 => assert_eq!(*i, 1),
                1 => assert_eq!(*i, 2),
                2 => assert_eq!(*i, 3),
                _ => assert!(false)
            }
        }
    }

    #[test]
    fn test_iter_overflow() {
        let mut buffer: RingBuffer<u8> = RingBuffer::new(3);
        buffer.push_back(1);
        buffer.push_back(2);
        buffer.push_back(3);
        buffer.push_back(4);
        buffer.push_back(5);
        for (idx, i) in buffer.iter().enumerate() {
            match idx {
                0 => assert_eq!(*i, 3),
                1 => assert_eq!(*i, 4),
                2 => assert_eq!(*i, 5),
                _ => assert!(false)
            }
        }
    }


    #[test]
    fn test_index() {
        let mut buffer: RingBuffer<u8> = RingBuffer::new(3);
        buffer.push_back(1);
        buffer.push_back(2);
        buffer.push_back(3);
        assert_eq!(buffer[0], 1);
        assert_eq!(buffer[1], 2);
        assert_eq!(buffer[2], 3);
    }


    #[test]
    #[should_panic]
    fn test_index_bounds() {
        let mut buffer: RingBuffer<u8> = RingBuffer::new(1);
        buffer.push_back(1);
        let _panic = buffer[1];
    }
}
