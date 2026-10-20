//! What the daemon and a running program say to each other, through two files in a
//! directory named after the executable's `LC_UUID` ([`dir`]):
//!
//! - `process`, which the program writes: who it is, which executable it runs, and where
//!   in its memory the next edit goes.
//! - `patch`, which the daemon writes: the code of the functions that differ from the
//!   executable, laid out for that place, or why the program has to be started again.
//!
//! Every address here is as the executable's load commands have it, before the loader
//! slid the program: the program adds its slide.

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"RAJITLV1";

/// The directory that holds one for each executable that runs.
pub fn root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|it| !it.is_empty())?;
    Some(Path::new(&home).join(".cache/ra-jit/live"))
}

/// The directory that the edits of the executable with `uuid` go through. It is named
/// after what the executable is, not where: cargo links a program in one place and leaves
/// a hard link of it in another, and a program may be copied and run from anywhere.
pub fn dir(uuid: &[u8; 16]) -> Option<PathBuf> {
    Some(root()?.join(hex(uuid)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

/// What the daemon has for a running program.
#[derive(Debug, PartialEq)]
pub struct Patch {
    /// The `LC_UUID` of the executable it is for.
    pub uuid: [u8; 16],
    /// Counts up with each patch the daemon writes for the executable.
    pub sequence: u64,
    pub body: Body,
}

#[derive(Debug, PartialEq)]
pub enum Body {
    Code(Code),
    /// The edit has no answer but a new executable, for this reason.
    Restart(String),
}

/// The functions that differ from the executable, and how to make it use them.
#[derive(Debug, PartialEq, Default)]
pub struct Code {
    /// Where the code goes: a multiple of the page size.
    pub base: u64,
    /// Read and run, from `base`. A multiple of the page size long.
    pub text: Vec<u8>,
    /// Read and written, behind `text`.
    pub data: Vec<u8>,
    /// Where `text` has the unwind tables of the code (`__eh_frame`), and how long.
    pub frames: (u64, u64),
    /// Where `text` and `data` store an address, as offsets from `base`.
    pub pointers: Vec<u64>,
    /// What changes in the executable: a jump over the entry of each function replaced,
    /// and the value of each static that has another.
    pub writes: Vec<Write>,
    /// How many of `writes` are jumps, and how many after them are statics, for the log.
    /// The rest put back what the executable had, where an earlier edit wrote.
    pub redirected: u64,
    pub statics: u64,
}

#[derive(Debug, PartialEq)]
pub struct Write {
    pub address: u64,
    pub bytes: Vec<u8>,
    /// Where `bytes` stores an address, as offsets into it.
    pub pointers: Vec<u64>,
}

impl Patch {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.uuid);
        put(&mut out, self.sequence);
        match &self.body {
            Body::Restart(why) => {
                put(&mut out, 0);
                bytes(&mut out, why.as_bytes());
            }
            Body::Code(code) => {
                put(&mut out, 1);
                put(&mut out, code.base);
                bytes(&mut out, &code.text);
                bytes(&mut out, &code.data);
                put(&mut out, code.frames.0);
                put(&mut out, code.frames.1);
                list(&mut out, &code.pointers);
                put(&mut out, code.redirected);
                put(&mut out, code.statics);
                put(&mut out, code.writes.len() as u64);
                for write in &code.writes {
                    put(&mut out, write.address);
                    bytes(&mut out, &write.bytes);
                    list(&mut out, &write.pointers);
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Patch, String> {
        let mut reader = Reader(bytes);
        if reader.take(8)? != MAGIC {
            return Err("not a patch of this version of ra-jit".to_owned());
        }
        let uuid = reader.take(16)?.try_into().map_err(|_| "a short patch")?;
        let sequence = reader.u64()?;
        let body = match reader.u64()? {
            0 => Body::Restart(String::from_utf8_lossy(reader.bytes()?).into_owned()),
            1 => {
                let base = reader.u64()?;
                let text = reader.bytes()?.to_vec();
                let data = reader.bytes()?.to_vec();
                let frames = (reader.u64()?, reader.u64()?);
                let pointers = reader.list()?;
                let redirected = reader.u64()?;
                let statics = reader.u64()?;
                let mut writes = Vec::new();
                for _ in 0..reader.u64()? {
                    let address = reader.u64()?;
                    let bytes = reader.bytes()?.to_vec();
                    writes.push(Write { address, bytes, pointers: reader.list()? });
                }
                Body::Code(Code { base, text, data, frames, pointers, writes, redirected, statics })
            }
            other => return Err(format!("a patch of kind {other}")),
        };
        Ok(Patch { uuid, sequence, body })
    }
}

/// What a running program says of itself.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Process {
    pub pid: u32,
    /// The `LC_UUID` of the executable it runs.
    pub uuid: [u8; 16],
    /// When it started, in milliseconds since the epoch.
    pub started: u64,
    /// Where the next patch may start, and where the room for patches ends.
    pub next: u64,
    pub end: u64,
}

impl Process {
    pub fn encode(&self) -> String {
        let uuid = hex(&self.uuid);
        format!(
            "pid {}\nuuid {uuid}\nstarted {}\nnext {:#x}\nend {:#x}\n",
            self.pid, self.started, self.next, self.end
        )
    }

    pub fn decode(text: &str) -> Option<Process> {
        let mut process = Process { pid: 0, uuid: [0; 16], started: 0, next: 0, end: 0 };
        let number = |it: &str| u64::from_str_radix(it.trim_start_matches("0x"), 16).ok();
        for line in text.lines() {
            let (key, value) = line.split_once(' ')?;
            match key {
                "pid" => process.pid = value.parse().ok()?,
                "uuid" => {
                    for (at, byte) in process.uuid.iter_mut().enumerate() {
                        *byte = u8::from_str_radix(value.get(at * 2..at * 2 + 2)?, 16).ok()?;
                    }
                }
                "started" => process.started = value.parse().ok()?,
                "next" => process.next = number(value)?,
                "end" => process.end = number(value)?,
                _ => {}
            }
        }
        (process.pid != 0).then_some(process)
    }
}

fn put(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn list(out: &mut Vec<u8>, values: &[u64]) {
    put(out, values.len() as u64);
    for value in values {
        put(out, *value);
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.0.len() < len {
            return Err("a patch that ends early".to_owned());
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| "a short patch")?))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let len = usize::try_from(self.u64()?).map_err(|_| "a patch too large")?;
        self.take(len)
    }

    fn list(&mut self) -> Result<Vec<u64>, String> {
        (0..self.u64()?).map(|_| self.u64()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let code = Code {
            base: 0x1_4000_0000,
            text: vec![1, 2, 3],
            data: vec![4],
            frames: (1, 2),
            pointers: vec![8, 16],
            writes: vec![Write { address: 0x1_0000_4000, bytes: vec![9; 4], pointers: vec![] }],
            redirected: 1,
            statics: 0,
        };
        let patch = Patch { uuid: [7; 16], sequence: 3, body: Body::Code(code) };
        assert_eq!(Patch::decode(&patch.encode()).unwrap(), patch);
        let restart = Patch { uuid: [1; 16], sequence: 4, body: Body::Restart("why".into()) };
        assert_eq!(Patch::decode(&restart.encode()).unwrap(), restart);
        let process = Process {
            pid: 42,
            uuid: [0xab; 16],
            started: 1_792_000_000_000,
            next: 0x1_8000_0000,
            end: 0x2_0000_0000,
        };
        assert_eq!(Process::decode(&process.encode()), Some(process));
    }
}
