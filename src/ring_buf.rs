#![allow(dead_code)]
use std::cmp::min;

pub struct RingBuf<T> {
    pub capacity: usize,
    init: T,
    read_pos: usize,
    write_pos: usize,
    buf_length: usize,
    pub buf: Box<[T]>,
}

impl<T: Clone + Copy> RingBuf<T> {
    pub fn new(capacity: usize, init: T) -> RingBuf<T> {
        let buf = vec!(init; capacity).into_boxed_slice();
        RingBuf {
            capacity,
            init,
            read_pos: 0usize,
            write_pos: 0usize,
            buf_length: 0usize,
            buf,
        }
    }

    pub fn len(&self) -> usize {
        self.buf_length
    }

    pub fn append(&mut self, new_slice: &[T]) {
        let mut length_to_copy = new_slice.len();
        let mut copied_length = 0usize;
    
        while length_to_copy > 0 {
            let length: usize = min(length_to_copy, self.capacity - self.write_pos);
            self.buf[self.write_pos .. self.write_pos + length]
                .copy_from_slice(&new_slice[copied_length .. copied_length + length]);

            length_to_copy -= length;
            copied_length += length;
            self.advance_write_head(length);
        }

        if self.write_pos > self.read_pos {
            self.buf_length = self.write_pos - self.read_pos;
        } else {
            self.buf_length = self.capacity + self.write_pos - self.read_pos;
        }
    }

    pub fn pop(&mut self, pop_buf: &mut [T]) {
        let n = pop_buf.len();
        if n > self.buf_length {
            panic!("Error! failed to pop buffer");
        }
 
        if self.write_pos > self.read_pos || self.capacity - self.read_pos >= n {
            pop_buf[..].copy_from_slice(&self.buf[self.read_pos .. self.read_pos + n]);
        } else {
            let i = self.capacity - self.read_pos;
            pop_buf[.. i].copy_from_slice(&self.buf[self.read_pos .. self.read_pos + i]);
            pop_buf[i ..].copy_from_slice(&self.buf[.. n - i]);
        }

        self.advance_read_head(n);
        if self.write_pos >= self.read_pos {
            self.buf_length = self.write_pos - self.read_pos;
        } else {
            self.buf_length = self.capacity + self.write_pos - self.read_pos;
        }
    }

    pub fn reset(&mut self) {
        self.buf_length = 0;
        self.read_pos = 0;
        self.write_pos = 0;
    }

    fn advance_read_head(&mut self, n: usize) {
        self.read_pos += n;
        if self.read_pos >= self.capacity {
            self.read_pos -= self.capacity;
        }
    }

    fn advance_write_head(&mut self, n: usize) {
        self.write_pos += n;
        if self.write_pos >= self.capacity {
            self.write_pos -= self.capacity;
        }
    }
}