//! CABAC arithmetic *encoder* (H.265 9.3.4.? encoding process), used to
//! regenerate the original slice data from decoded bins.

pub struct CabacEnc {
    low: u32,
    range: u32,
    first_bit: bool,
    outstanding: u32,
    out: Vec<u8>,
    cur: u8,
    nbits: u32,
}

impl CabacEnc {
    pub fn new() -> CabacEnc {
        CabacEnc { low: 0, range: 510, first_bit: true, outstanding: 0, out: Vec::new(), cur: 0, nbits: 0 }
    }

    pub fn range(&self) -> u32 {
        self.range
    }

    fn write_bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | b as u8;
        self.nbits += 1;
        if self.nbits == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    fn put_bit(&mut self, b: u32) {
        if self.first_bit {
            self.first_bit = false;
        } else {
            self.write_bit(b);
        }
        while self.outstanding > 0 {
            self.write_bit(1 - b);
            self.outstanding -= 1;
        }
    }

    fn renorm(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put_bit(0);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put_bit(1);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    /// Encode a context-coded bin; `lps` is rangeTabLps for the current state.
    pub fn encode(&mut self, lps: u32, is_lps: bool) {
        self.range -= lps;
        if is_lps {
            self.low += self.range;
            self.range = lps;
        }
        self.renorm();
    }

    pub fn encode_bypass(&mut self, bin: u32) {
        self.low <<= 1;
        if bin == 1 {
            self.low += self.range;
        }
        if self.low >= 1024 {
            self.put_bit(1);
            self.low -= 1024;
        } else if self.low < 512 {
            self.put_bit(0);
        } else {
            self.low -= 512;
            self.outstanding += 1;
        }
    }

    pub fn encode_terminate(&mut self, bin: u32) {
        self.range -= 2;
        if bin == 1 {
            self.low += self.range;
            // Flush.
            self.range = 2;
            self.renorm();
            self.put_bit((self.low >> 9) & 1);
            let v = ((self.low >> 7) & 3) | 1;
            self.write_bit((v >> 1) & 1);
            self.write_bit(v & 1);
        } else {
            self.renorm();
        }
    }

    /// Pad with zero bits to a byte boundary and start a new arithmetic code
    /// word (next substream, or the end of the slice).
    pub fn align_and_restart(&mut self) {
        while self.nbits != 0 {
            self.write_bit(0);
        }
        self.low = 0;
        self.range = 510;
        self.first_bit = true;
        self.outstanding = 0;
    }

    /// Bytes produced so far; call after `align_and_restart` at the end of a slice.
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}
