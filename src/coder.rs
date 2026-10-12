//! Binary arithmetic coder for the model-coded stream (32-bit, carry-less,
//! 16-bit probabilities). Integer only, so encoder and decoder agree exactly.

pub struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder {
            x1: 0,
            x2: 0xFFFF_FFFF,
            out: Vec::new(),
        }
    }

    /// Encode `bin` given P(bin = 1) = p16 / 65536, with 0 < p16 < 65536.
    pub fn encode(&mut self, bin: u32, p16: u32) {
        let xmid = self.x1 + (((self.x2 - self.x1) as u64 * p16 as u64) >> 16) as u32;
        if bin == 1 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xFF00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.out.extend_from_slice(&self.x1.to_be_bytes());
        self.out
    }
}

pub struct Decoder {
    x1: u32,
    x2: u32,
    x: u32,
    data: Vec<u8>,
    pos: usize,
}

impl Decoder {
    pub fn new(data: Vec<u8>) -> Decoder {
        let mut d = Decoder {
            x1: 0,
            x2: 0xFFFF_FFFF,
            x: 0,
            data,
            pos: 0,
        };
        for _ in 0..4 {
            d.x = (d.x << 8) | d.next_byte();
        }
        d
    }

    fn next_byte(&mut self) -> u32 {
        let b = self.data.get(self.pos).copied().unwrap_or(0) as u32;
        self.pos += 1;
        b
    }

    pub fn decode(&mut self, p16: u32) -> u32 {
        let xmid = self.x1 + (((self.x2 - self.x1) as u64 * p16 as u64) >> 16) as u32;
        let bin = if self.x <= xmid {
            self.x2 = xmid;
            1
        } else {
            self.x1 = xmid + 1;
            0
        };
        while (self.x1 ^ self.x2) & 0xFF00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
            self.x = (self.x << 8) | self.next_byte();
        }
        bin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut state = 12345u32;
        let mut rnd = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            state >> 8
        };
        let items: Vec<(u32, u32)> = (0..20000)
            .map(|_| {
                let p = (rnd() % 65520).max(16);
                let bin = ((rnd() % 65536) < p) as u32;
                (bin, p)
            })
            .collect();
        let mut e = Encoder::new();
        for &(b, p) in &items {
            e.encode(b, p);
        }
        let bytes = e.finish();
        let mut d = Decoder::new(bytes);
        for &(b, p) in &items {
            assert_eq!(d.decode(p), b);
        }
    }
}
