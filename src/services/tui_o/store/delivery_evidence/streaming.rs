//! Validate oversized non-delivery rows without retaining their payloads.

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use std::{
    cell::Cell,
    fmt,
    io::{self, BufRead, Read},
};
use tokio::time::Instant;

#[cfg(test)]
thread_local! {
    pub(super) static EXPIRE_AFTER_CHUNK: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn notice<R: BufRead>(reader: R, deadline: Instant) -> Option<()> {
    let capped = Cell::new(true);
    let mut input = Strict {
        reader,
        deadline,
        capped: &capped,
        done: false,
        offset: 0,
        quoted: false,
        string_bytes: 0,
        escape: 0,
        hex: 0,
        depth: 0,
        utf8: [0; 4],
        utf8_len: 0,
        utf8_need: 0,
    };
    let mut parser = serde_json::Deserializer::from_reader(&mut input);
    Header(&capped, 0).deserialize(&mut parser).ok()?;
    parser.end().ok()?;
    input.done.then_some(())
}

struct Header<'a>(&'a Cell<bool>, u8);

impl<'de> DeserializeSeed<'de> for Header<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Header<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a known input notice ledger row")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = 0u8;
        loop {
            self.0.set(true);
            let Some(key) = map.next_key::<String>()? else {
                break;
            };
            let bit = match (self.1, key.as_str()) {
                (0, "at") => {
                    let _: chrono::DateTime<chrono::Utc> = map.next_value()?;
                    1
                }
                (0, "entry") | (1, "entry") => {
                    map.next_value_seed(Header(self.0, self.1 + 1))?;
                    2
                }
                (1, "type") => {
                    if map.next_value::<String>()? != "input_notice" {
                        return Err(de::Error::custom("oversized delivery row"));
                    }
                    1
                }
                (2, "kind") => {
                    let kind = map.next_value::<String>()?;
                    if !matches!(
                        kind.as_str(),
                        "materialized"
                            | "migration_started"
                            | "legacy_imported"
                            | "migration_unresolved"
                    ) {
                        return Err(de::Error::custom("unknown input notice row"));
                    }
                    1
                }
                _ => {
                    self.0.set(false);
                    map.next_value::<IgnoredAny>()?;
                    0
                }
            };
            if seen & bit != 0 {
                return Err(de::Error::custom("duplicate ledger field"));
            }
            seen |= bit;
        }
        if seen != if self.1 == 2 { 1 } else { 3 } {
            return Err(de::Error::custom("incomplete input notice header"));
        }
        Ok(())
    }
}

struct Strict<'a, R> {
    reader: R,
    deadline: Instant,
    capped: &'a Cell<bool>,
    done: bool,
    offset: usize,
    quoted: bool,
    string_bytes: usize,
    escape: u8,
    hex: u16,
    depth: u8,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid streamed ledger row")
}

impl<R: BufRead> Read for Strict<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.done {
            return Ok(0);
        }
        if self.offset % 4096 == 0 {
            #[cfg(test)]
            if self.offset != 0 && EXPIRE_AFTER_CHUNK.with(|expire| expire.replace(false)) {
                self.deadline = Instant::now();
            }
            if Instant::now() >= self.deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "ledger snapshot deadline",
                ));
            }
        }
        let available = self.reader.fill_buf()?;
        if available.is_empty() {
            return Err(invalid());
        }
        let limit = available
            .len()
            .min(out.len())
            .min(4096 - self.offset % 4096);
        let count = available[..limit]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(limit, |n| n + 1);
        out[..count].copy_from_slice(&available[..count]);
        self.reader.consume(count);
        for &byte in &out[..count] {
            self.byte(byte)?;
        }
        self.offset += count;
        Ok(count)
    }
}

impl<R> Strict<'_, R> {
    fn byte(&mut self, byte: u8) -> io::Result<()> {
        if self.quoted {
            self.string_bytes = self.string_bytes.checked_add(1).ok_or_else(invalid)?;
            if self.capped.get() && self.string_bytes > 128 {
                return Err(invalid());
            }
        }
        if self.utf8_need != 0 {
            if !(0x80..=0xbf).contains(&byte) {
                return Err(invalid());
            }
            self.utf8[self.utf8_len] = byte;
            self.utf8_len += 1;
            if self.utf8_len == self.utf8_need {
                std::str::from_utf8(&self.utf8[..self.utf8_len]).map_err(|_| invalid())?;
                self.utf8_need = 0;
            }
        } else if self.quoted {
            match self.escape {
                1 => match byte {
                    b'u' => {
                        self.escape = 2;
                        self.hex = 0;
                    }
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.escape = 0,
                    _ => return Err(invalid()),
                },
                2..=5 | 8..=11 => {
                    let digit = (byte as char).to_digit(16).ok_or_else(invalid)?;
                    self.hex = (self.hex << 4) | digit as u16;
                    self.escape += 1;
                    if self.escape == 6 {
                        self.escape = if (0xd800..=0xdbff).contains(&self.hex) {
                            6
                        } else if (0xdc00..=0xdfff).contains(&self.hex) {
                            return Err(invalid());
                        } else {
                            0
                        };
                    } else if self.escape == 12 {
                        if !(0xdc00..=0xdfff).contains(&self.hex) {
                            return Err(invalid());
                        }
                        self.escape = 0;
                    }
                }
                6 if byte == b'\\' => self.escape = 7,
                7 if byte == b'u' => {
                    self.escape = 8;
                    self.hex = 0;
                }
                6 | 7 => return Err(invalid()),
                _ => match byte {
                    b'"' => self.quoted = false,
                    b'\\' => self.escape = 1,
                    0..=0x1f => return Err(invalid()),
                    0x80..=0xff => {
                        self.utf8[0] = byte;
                        self.utf8_len = 1;
                        self.utf8_need = match byte {
                            0xc2..=0xdf => 2,
                            0xe0..=0xef => 3,
                            0xf0..=0xf4 => 4,
                            _ => return Err(invalid()),
                        };
                    }
                    _ => {}
                },
            }
        } else {
            match byte {
                b'"' => {
                    self.quoted = true;
                    self.string_bytes = 0;
                }
                b'{' | b'[' => {
                    self.depth = self.depth.checked_add(1).ok_or_else(invalid)?;
                    if self.depth > 128 {
                        return Err(invalid());
                    }
                }
                b'}' | b']' => self.depth = self.depth.checked_sub(1).ok_or_else(invalid)?,
                b'\n' => {
                    if self.depth != 0 {
                        return Err(invalid());
                    }
                    self.done = true;
                }
                0x80..=0xff => return Err(invalid()),
                _ => {}
            }
        }
        Ok(())
    }
}
