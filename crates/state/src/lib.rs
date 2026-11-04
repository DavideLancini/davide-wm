//! Serialisation for the live upgrade (README, ADR-014).
//!
//! The running process encodes its whole state with [`Persist`], keeps the
//! file descriptors it references open across `execve` (their numbers are
//! stored in the blob) and the new binary decodes it. Every struct lists all
//! of its fields through [`persist_struct!`], so adding a field without
//! deciding how it survives an upgrade does not compile.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::hash::Hash;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "state blob: {}", self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Default)]
pub struct Enc {
    pub buf: Vec<u8>,
    /// Descriptors referenced by the blob; they must survive the exec.
    pub fds: Vec<RawFd>,
}

pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8]) -> Dec<'a> {
        Dec { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let b = self.buf.get(self.pos..self.pos + n).ok_or_else(|| Error("truncated".into()))?;
        self.pos += n;
        Ok(b)
    }

    pub fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

pub trait Persist: Sized {
    fn save(&self, e: &mut Enc);
    fn load(d: &mut Dec) -> Result<Self>;
}

macro_rules! int {
    ($($t:ty),*) => {$(
        impl Persist for $t {
            fn save(&self, e: &mut Enc) {
                e.buf.extend_from_slice(&self.to_le_bytes());
            }
            fn load(d: &mut Dec) -> Result<Self> {
                Ok(<$t>::from_le_bytes(d.take(std::mem::size_of::<$t>())?.try_into().unwrap()))
            }
        }
    )*};
}

int!(u8, u16, u32, u64, i32, i64, f32, f64);

impl Persist for usize {
    fn save(&self, e: &mut Enc) {
        (*self as u64).save(e)
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(u64::load(d)? as usize)
    }
}

impl Persist for bool {
    fn save(&self, e: &mut Enc) {
        (*self as u8).save(e)
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(u8::load(d)? != 0)
    }
}

/// Byte buffers in one copy; `Vec<u8>` through `Persist` goes byte by byte.
pub fn save_bytes(b: &[u8], e: &mut Enc) {
    b.len().save(e);
    e.buf.extend_from_slice(b);
}

pub fn load_bytes(d: &mut Dec) -> Result<Vec<u8>> {
    let n = usize::load(d)?;
    Ok(d.take(n)?.to_vec())
}

impl Persist for String {
    fn save(&self, e: &mut Enc) {
        save_bytes(self.as_bytes(), e)
    }
    fn load(d: &mut Dec) -> Result<Self> {
        String::from_utf8(load_bytes(d)?).map_err(|_| Error("bad utf-8".into()))
    }
}

impl<T: Persist> Persist for Option<T> {
    fn save(&self, e: &mut Enc) {
        match self {
            None => 0u8.save(e),
            Some(v) => {
                1u8.save(e);
                v.save(e);
            }
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(match u8::load(d)? {
            0 => None,
            _ => Some(T::load(d)?),
        })
    }
}

impl<T: Persist> Persist for Vec<T> {
    fn save(&self, e: &mut Enc) {
        self.len().save(e);
        for v in self {
            v.save(e);
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let n = usize::load(d)?;
        if n > d.buf.len() {
            return Err(Error("length out of range".into()));
        }
        (0..n).map(|_| T::load(d)).collect()
    }
}

impl<T: Persist> Persist for VecDeque<T> {
    fn save(&self, e: &mut Enc) {
        self.len().save(e);
        for v in self {
            v.save(e);
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(Vec::<T>::load(d)?.into())
    }
}

impl<T: Persist> Persist for Box<T> {
    fn save(&self, e: &mut Enc) {
        (**self).save(e)
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(Box::new(T::load(d)?))
    }
}

impl<K: Persist + Eq + Hash, V: Persist> Persist for HashMap<K, V> {
    fn save(&self, e: &mut Enc) {
        self.len().save(e);
        for (k, v) in self {
            k.save(e);
            v.save(e);
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let n = usize::load(d)?;
        let mut m = HashMap::with_capacity(n.min(4096));
        for _ in 0..n {
            let k = K::load(d)?;
            m.insert(k, V::load(d)?);
        }
        Ok(m)
    }
}

impl<T: Persist + Eq + Hash> Persist for HashSet<T> {
    fn save(&self, e: &mut Enc) {
        self.len().save(e);
        for v in self {
            v.save(e);
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let n = usize::load(d)?;
        (0..n).map(|_| T::load(d)).collect()
    }
}

impl<A: Persist, B: Persist> Persist for (A, B) {
    fn save(&self, e: &mut Enc) {
        self.0.save(e);
        self.1.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok((A::load(d)?, B::load(d)?))
    }
}

impl<A: Persist, B: Persist, C: Persist> Persist for (A, B, C) {
    fn save(&self, e: &mut Enc) {
        self.0.save(e);
        self.1.save(e);
        self.2.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok((A::load(d)?, B::load(d)?, C::load(d)?))
    }
}

impl<A: Persist, B: Persist, C: Persist, D: Persist> Persist for (A, B, C, D) {
    fn save(&self, e: &mut Enc) {
        self.0.save(e);
        self.1.save(e);
        self.2.save(e);
        self.3.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok((A::load(d)?, B::load(d)?, C::load(d)?, D::load(d)?))
    }
}

impl<A: Persist, B: Persist, C: Persist, D: Persist, E: Persist> Persist for (A, B, C, D, E) {
    fn save(&self, e: &mut Enc) {
        self.0.save(e);
        self.1.save(e);
        self.2.save(e);
        self.3.save(e);
        self.4.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok((A::load(d)?, B::load(d)?, C::load(d)?, D::load(d)?, E::load(d)?))
    }
}

/// The descriptor number is stored as is and kept open across exec.
impl Persist for OwnedFd {
    fn save(&self, e: &mut Enc) {
        let fd = self.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
        e.fds.push(fd);
        fd.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let fd = i32::load(d)?;
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(Error(format!("fd {fd} did not survive the exec")));
        }
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// Saves a descriptor owned by something that is not an `OwnedFd`; load it
/// back as an `OwnedFd`.
pub fn save_fd(fd: &impl AsRawFd, e: &mut Enc) {
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }
    e.fds.push(raw);
    raw.save(e);
}

/// Implements [`Persist`] for a struct by listing every field.
#[macro_export]
macro_rules! persist_struct {
    ($t:ident { $($f:ident),* $(,)? }) => {
        impl $crate::Persist for $t {
            fn save(&self, e: &mut $crate::Enc) {
                $($crate::Persist::save(&self.$f, e);)*
            }
            fn load(d: &mut $crate::Dec) -> $crate::Result<Self> {
                Ok($t { $($f: $crate::Persist::load(d)?),* })
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct S {
        a: u32,
        b: Vec<String>,
        c: Option<(i32, bool)>,
        d: HashMap<u32, f64>,
    }
    persist_struct!(S { a, b, c, d });

    #[test]
    fn roundtrip() {
        let s = S { a: 7, b: vec!["x".into(), "yz".into()], c: Some((-3, true)), d: [(1, 0.5)].into() };
        let mut e = Enc::default();
        s.save(&mut e);
        let mut d = Dec::new(&e.buf);
        let t = S::load(&mut d).unwrap();
        assert!(d.done());
        assert_eq!((t.a, t.b, t.c, t.d), (7, vec!["x".to_string(), "yz".to_string()], Some((-3, true)), [(1, 0.5)].into()));
    }
}
