pub struct RingBuffer<T: Copy, const N: usize> {
    buf:  [T; N],
    head: usize,
    len:  usize,
}

impl<T: Copy, const N: usize> RingBuffer<T, N> {
    pub const fn new(initial: T) -> Self {
        Self {
            buf:  [initial; N],
            head: 0,
            len:  0,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.len < N {
            let tail = (self.head + self.len) % N;
            self.buf[tail] = item;
            self.len += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    // Copies `min(self.len, n)` items from head into `dst` without advancing head.
    // Returns the number of items copied.
    pub unsafe fn peek_into(&self, dst: *mut T, n: usize) -> usize {
        let count = self.len.min(n);
        for i in 0..count {
            let idx = (self.head + i) % N;
            unsafe { dst.add(i).write(self.buf[idx]); }
        }
        count
    }

    pub fn advance(&mut self, n: usize) {
        let n = n.min(self.len);
        self.head = (self.head + n) % N;
        self.len -= n;
    }

    pub unsafe fn dequeue_into(&mut self, dst: *mut T, n: usize) {
        let copied = unsafe { self.peek_into(dst, n) };
        self.advance(copied);
    }

    // Returns the most recently pushed item without removing it.
    pub fn peek_back(&self) -> Option<T> {
        if self.len == 0 {
            None
        } else {
            Some(self.buf[(self.head + self.len - 1) % N])
        }
    }

    // Removes and returns the most recently pushed item.
    pub fn pop_back(&mut self) -> Option<T> {
        let item = self.peek_back();
        if item.is_some() {
            self.len -= 1;
        }
        item
    }
}
