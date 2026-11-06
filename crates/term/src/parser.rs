//! DEC-compatible parser after Paul Williams' VT500 state diagram, with UTF-8
//! decoding in the ground state and colon subparameters (ITU T.416) in CSI.
//! DCS, SOS, PM and APC strings are consumed and dropped.

const MAX_PARAMS: usize = 32;
const MAX_INTER: usize = 2;
const MAX_OSC: usize = 4 << 20;

pub(crate) trait Perform {
    fn print(&mut self, c: char);
    fn execute(&mut self, b: u8);
    fn csi(&mut self, p: &Params, inter: &[u8], fin: u8);
    fn esc(&mut self, inter: &[u8], fin: u8);
    /// `bel`: terminated by BEL rather than ST; replies mirror it.
    fn osc(&mut self, data: &[u8], bel: bool);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscInter,
    CsiEntry,
    CsiParam,
    CsiInter,
    CsiIgnore,
    Osc,
    DcsEntry,
    DcsParam,
    DcsInter,
    DcsPass,
    DcsIgnore,
    String,
}

/// CSI parameters; a group is a parameter with its colon subparameters.
#[derive(Default)]
pub(crate) struct Params {
    vals: [u16; MAX_PARAMS],
    /// Bit i: value i follows a colon.
    sub: u32,
    len: usize,
}

impl Params {
    fn clear(&mut self) {
        self.len = 0;
        self.sub = 0;
    }

    fn digit(&mut self, d: u8) {
        if self.len == 0 {
            self.vals[0] = 0;
            self.len = 1;
        }
        let v = &mut self.vals[self.len - 1];
        *v = v.saturating_mul(10).saturating_add(d as u16);
    }

    fn sep(&mut self, colon: bool) {
        if self.len == 0 {
            self.vals[0] = 0;
            self.len = 1;
        }
        if self.len < MAX_PARAMS {
            self.vals[self.len] = 0;
            if colon {
                self.sub |= 1 << self.len;
            }
            self.len += 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn groups(&self) -> impl Iterator<Item = &[u16]> {
        let mut i = 0;
        std::iter::from_fn(move || {
            if i >= self.len {
                return None;
            }
            let s = i;
            i += 1;
            while i < self.len && self.sub & (1 << i) != 0 {
                i += 1;
            }
            Some(&self.vals[s..i])
        })
    }

    /// Parameter `i` (first value of group `i`), or `def` if missing or 0.
    pub fn get(&self, i: usize, def: u16) -> u16 {
        match self.groups().nth(i).map(|g| g[0]) {
            Some(0) | None => def,
            Some(v) => v,
        }
    }

    /// Parameter `i` with 0 kept.
    pub fn raw(&self, i: usize) -> Option<u16> {
        self.groups().nth(i).map(|g| g[0])
    }
}

#[derive(Default)]
pub(crate) struct Parser {
    state: State,
    params: Params,
    inter: [u8; MAX_INTER],
    ninter: usize,
    ignore: bool,
    osc: Vec<u8>,
    osc_overflow: bool,
    /// UTF-8: code point so far, continuation bytes left, minimum value.
    cp: u32,
    need: u8,
    min: u32,
}

impl Parser {
    pub fn advance<P: Perform>(&mut self, p: &mut P, b: u8) {
        if self.need > 0 {
            if b & 0xc0 == 0x80 {
                self.cp = self.cp << 6 | (b & 0x3f) as u32;
                self.need -= 1;
                if self.need == 0 {
                    let c = match char::from_u32(self.cp) {
                        Some(c) if self.cp >= self.min => c,
                        _ => '\u{fffd}',
                    };
                    if !('\u{80}'..'\u{a0}').contains(&c) {
                        p.print(c);
                    }
                }
                return;
            }
            self.need = 0;
            p.print('\u{fffd}');
        }
        match b {
            0x18 | 0x1a => {
                if b == 0x1a && self.state != State::Ground {
                    p.print('\u{fffd}');
                }
                self.state = State::Ground;
                return;
            }
            0x1b => {
                if self.state == State::Osc {
                    self.end_osc(p, false);
                }
                self.enter(State::Escape);
                return;
            }
            _ => {}
        }
        match self.state {
            State::Ground => match b {
                0x00..=0x1f => p.execute(b),
                0x20..=0x7e => p.print(b as char),
                0x7f => {}
                0xc2..=0xdf => self.utf8(b & 0x1f, 1, 0x80),
                0xe0..=0xef => self.utf8(b & 0x0f, 2, 0x800),
                0xf0..=0xf4 => self.utf8(b & 0x07, 3, 0x10000),
                _ => p.print('\u{fffd}'),
            },
            State::Escape => match b {
                0x00..=0x1f => p.execute(b),
                0x20..=0x2f => {
                    self.collect(b);
                    self.state = State::EscInter;
                }
                b'[' => self.enter(State::CsiEntry),
                b']' => {
                    self.osc.clear();
                    self.osc_overflow = false;
                    self.state = State::Osc;
                }
                b'P' => self.enter(State::DcsEntry),
                b'X' | b'^' | b'_' => self.state = State::String,
                0x30..=0x7e => {
                    self.state = State::Ground;
                    p.esc(&self.inter[..self.ninter], b);
                }
                _ => {}
            },
            State::EscInter => match b {
                0x00..=0x1f => p.execute(b),
                0x20..=0x2f => self.collect(b),
                0x30..=0x7e => {
                    self.state = State::Ground;
                    if !self.ignore {
                        p.esc(&self.inter[..self.ninter], b);
                    }
                }
                _ => {}
            },
            State::CsiEntry | State::CsiParam => match b {
                0x00..=0x1f => p.execute(b),
                b'0'..=b'9' => {
                    self.params.digit(b - b'0');
                    self.state = State::CsiParam;
                }
                b':' | b';' => {
                    self.params.sep(b == b':');
                    self.state = State::CsiParam;
                }
                0x3c..=0x3f => {
                    if self.state == State::CsiEntry {
                        self.collect(b);
                        self.state = State::CsiParam;
                    } else {
                        self.state = State::CsiIgnore;
                    }
                }
                0x20..=0x2f => {
                    self.collect(b);
                    self.state = State::CsiInter;
                }
                0x40..=0x7e => self.dispatch_csi(p, b),
                _ => {}
            },
            State::CsiInter => match b {
                0x00..=0x1f => p.execute(b),
                0x20..=0x2f => self.collect(b),
                0x30..=0x3f => self.state = State::CsiIgnore,
                0x40..=0x7e => self.dispatch_csi(p, b),
                _ => {}
            },
            State::CsiIgnore => match b {
                0x00..=0x1f => p.execute(b),
                0x40..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::Osc => match b {
                0x07 => self.end_osc(p, true),
                0x00..=0x1f => {}
                _ => {
                    if self.osc.len() < MAX_OSC {
                        self.osc.push(b);
                    } else {
                        self.osc_overflow = true;
                    }
                }
            },
            State::DcsEntry | State::DcsParam => match b {
                0x30..=0x39 | b';' => self.state = State::DcsParam,
                b':' => self.state = State::DcsIgnore,
                0x3c..=0x3f if self.state == State::DcsEntry => self.state = State::DcsParam,
                0x3c..=0x3f => self.state = State::DcsIgnore,
                0x20..=0x2f => self.state = State::DcsInter,
                0x40..=0x7e => self.state = State::DcsPass,
                _ => {}
            },
            State::DcsInter => match b {
                0x30..=0x3f => self.state = State::DcsIgnore,
                0x40..=0x7e => self.state = State::DcsPass,
                _ => {}
            },
            State::DcsPass | State::DcsIgnore | State::String => {}
        }
    }

    fn utf8(&mut self, bits: u8, need: u8, min: u32) {
        self.cp = bits as u32;
        self.need = need;
        self.min = min;
    }

    fn enter(&mut self, s: State) {
        self.state = s;
        self.params.clear();
        self.ninter = 0;
        self.ignore = false;
    }

    fn collect(&mut self, b: u8) {
        if self.ninter < MAX_INTER {
            self.inter[self.ninter] = b;
            self.ninter += 1;
        } else {
            self.ignore = true;
        }
    }

    fn dispatch_csi<P: Perform>(&mut self, p: &mut P, fin: u8) {
        self.state = State::Ground;
        if !self.ignore {
            p.csi(&self.params, &self.inter[..self.ninter], fin);
        }
    }

    fn end_osc<P: Perform>(&mut self, p: &mut P, bel: bool) {
        self.state = State::Ground;
        if !self.osc_overflow {
            p.osc(&self.osc, bel);
        }
        if self.osc.capacity() > 4096 {
            self.osc = Vec::new();
        }
        self.osc.clear();
    }
}
