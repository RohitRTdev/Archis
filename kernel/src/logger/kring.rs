use crate::sync::Spinlock;

const KRING_CAPACITY: usize = 64 * 1024; // 64KB
const MAX_MSG_COPY: usize = 1024; // 1KB

struct KernelRingBuf {
    buf:  [u8; KRING_CAPACITY],
    head: usize,
    len:  usize
}

impl KernelRingBuf {
    const fn new() -> Self {
        Self { buf: [0; KRING_CAPACITY], head: 0, len: 0 }
    }

    fn free(&self) -> usize {
        KRING_CAPACITY - self.len
    }

    fn push_byte(&mut self, b: u8) {
        let tail = (self.head + self.len) % KRING_CAPACITY;
        self.buf[tail] = b;
        self.len += 1;
    }

    fn read_byte(&self, offset: usize) -> u8 {
        self.buf[(self.head + offset) % KRING_CAPACITY]
    }

    // Pop the current head message (if it exists)
    fn drop_oldest(&mut self) {
        if self.len < 2 {
            self.head = 0;
            self.len = 0;
            return;
        }
        let msg_len = u16::from_le_bytes([self.read_byte(0), self.read_byte(1)]) as usize;
        let total = 2 + msg_len;
        let advance = total.min(self.len);
        self.head = (self.head + advance) % KRING_CAPACITY;
        self.len -= advance;
    }

    // Each message has starts with a 2 byte length prefix followed by the message
    // We silently drop the message if its greater than kring max capacity
    fn push_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let needed = 2 + bytes.len();
        if needed > KRING_CAPACITY {
            return;
        }
        while self.free() < needed {
            self.drop_oldest();
        }
        let [lo, hi] = (bytes.len() as u16).to_le_bytes();
        self.push_byte(lo);
        self.push_byte(hi);
        for &b in bytes {
            self.push_byte(b);
        }
    }

    fn for_each_msg<F: FnMut(&str)>(&self, mut f: F) {
        let mut pos = 0usize;
        let mut remaining = self.len;
        let mut tmp = [0u8; MAX_MSG_COPY];
        while remaining >= 2 {
            let msg_len = u16::from_le_bytes([
                self.read_byte(pos),
                self.read_byte(pos + 1)
            ]) as usize;
            pos += 2;
            remaining -= 2;
            if msg_len > remaining {
                break;
            }
            let copy_len = msg_len.min(MAX_MSG_COPY);
            for i in 0..copy_len {
                tmp[i] = self.read_byte(pos + i);
            }

            // If the message is valid utf-8, call the user provided closure
            // with this argument
            if let Ok(s) = core::str::from_utf8(&tmp[..copy_len]) {
                f(s);
            }
            pos += msg_len;
            remaining -= msg_len;
        }
    }
}

static KRING: Spinlock<KernelRingBuf> = Spinlock::new(KernelRingBuf::new());

pub(super) fn push(s: &str) {
    KRING.lock().push_str(s);
}

pub fn kring_log_for_each<F: FnMut(&str)>(f: F) {
    KRING.lock().for_each_msg(f);
}
