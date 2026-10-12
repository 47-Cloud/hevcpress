/// MSB-first bit reader over an RBSP. Reads past the end yield zeros.
pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    pub fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos / 8).copied().unwrap_or(0);
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        b as u32
    }

    pub fn u(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Unsigned Exp-Golomb.
    pub fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 && zeros < 32 {
            zeros += 1;
        }
        if zeros == 0 {
            return 0;
        }
        ((1u64 << zeros) - 1 + self.u(zeros) as u64) as u32
    }

    /// Signed Exp-Golomb.
    pub fn se(&mut self) -> i32 {
        let k = self.ue() as i64;
        (if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) }) as i32
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    pub fn exhausted(&self) -> bool {
        self.pos > self.data.len() * 8
    }
}

/// Strip emulation-prevention bytes (00 00 03 -> 00 00).
pub fn unescape(nal_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_payload.len());
    let mut zeros = 0;
    for &b in nal_payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}
