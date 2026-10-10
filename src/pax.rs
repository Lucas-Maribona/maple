//! Adapt binary PAX xattrs to tar's newline-based extension parser. The wire
//! format remains standard PAX; only this in-memory stream uses hex values.
use std::io::{self, Cursor, Read};
use tar::Header;

pub struct Reader<R> {
    input: R,
    pending: Cursor<Vec<u8>>,
    remaining: u64,
    next_size: Option<u64>,
    ended: bool,
}

impl<R: Read> Reader<R> {
    pub fn new(input: R) -> Self {
        Self {
            input,
            pending: Cursor::new(Vec::new()),
            remaining: 0,
            next_size: None,
            ended: false,
        }
    }

    fn block(&mut self) -> io::Result<()> {
        let mut bytes = [0; 512];
        let count = self.input.read(&mut bytes[..1])?;
        if count == 0 {
            self.ended = true;
            return Ok(());
        }
        self.input.read_exact(&mut bytes[1..])?;
        if bytes.iter().all(|b| *b == 0) {
            self.pending = Cursor::new(bytes.to_vec());
            return Ok(());
        }
        let mut header = Header::from_byte_slice(&bytes).clone();
        let kind = header.entry_type();
        let size = header.entry_size()?;
        if kind.is_pax_local_extensions() || kind.is_pax_global_extensions() {
            // Do not let rewriting a header hide an invalid input checksum.
            let checksum: u32 = bytes
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if (148..156).contains(&i) {
                        32
                    } else {
                        u32::from(*b)
                    }
                })
                .sum();
            if checksum != header.cksum()? {
                return Err(invalid("invalid PAX checksum"));
            }
            if size > 16 * 1024 * 1024 {
                return Err(invalid("PAX metadata exceeds 16 MiB"));
            }
            let mut data = vec![0; size as usize];
            self.input.read_exact(&mut data)?;
            let mut padding = vec![0; ((512 - size % 512) % 512) as usize];
            self.input.read_exact(&mut padding)?;
            let mut normalized = Vec::new();
            for (key, value) in records(&data)? {
                if key == "size" {
                    self.next_size = Some(
                        std::str::from_utf8(value)
                            .map_err(invalid)?
                            .parse()
                            .map_err(invalid)?,
                    );
                }
                if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
                    let value: String = value.iter().map(|b| format!("{b:02x}")).collect();
                    append(
                        &mut normalized,
                        &format!("MAPLE.xattr.hex.{name}"),
                        value.as_bytes(),
                    );
                } else if key == "SCHILY.acl.access" || key == "SCHILY.acl.default" {
                    let value: Vec<_> = value
                        .iter()
                        .map(|b| if *b == b'\n' { b',' } else { *b })
                        .collect();
                    append(&mut normalized, key, &value);
                } else {
                    if value.contains(&b'\n') {
                        return Err(invalid("newline in non-xattr PAX value is unsupported"));
                    }
                    append(&mut normalized, key, value);
                }
            }
            header.set_size(normalized.len() as u64);
            header.set_cksum();
            let mut output = header.as_bytes().to_vec();
            output.extend(&normalized);
            output.resize(output.len().div_ceil(512) * 512, 0);
            self.pending = Cursor::new(output);
        } else {
            let size = if kind.is_gnu_longname() || kind.is_gnu_longlink() {
                size
            } else {
                self.next_size.take().unwrap_or(size)
            };
            self.remaining = size
                .checked_add(511)
                .ok_or_else(|| invalid("tar member too large"))?
                / 512
                * 512;
            self.pending = Cursor::new(bytes.to_vec());
        }
        Ok(())
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.pending.read(buffer)?;
            if count != 0 {
                return Ok(count);
            }
            if self.remaining != 0 {
                let limit = self.remaining.min(buffer.len() as u64) as usize;
                let count = self.input.read(&mut buffer[..limit])?;
                if count == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                self.remaining -= count as u64;
                return Ok(count);
            }
            if self.ended {
                return Ok(0);
            }
            self.block()?;
        }
    }
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn records(mut data: &[u8]) -> io::Result<Vec<(&str, &[u8])>> {
    let mut result = Vec::new();
    while !data.is_empty() {
        let space = data
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| invalid("invalid PAX length"))?;
        let length: usize = std::str::from_utf8(&data[..space])
            .map_err(invalid)?
            .parse()
            .map_err(invalid)?;
        if length <= space + 2 || length > data.len() || data[length - 1] != b'\n' {
            return Err(invalid("invalid PAX record length"));
        }
        let record = &data[space + 1..length - 1];
        let equals = record
            .iter()
            .position(|b| *b == b'=')
            .ok_or_else(|| invalid("invalid PAX key"))?;
        let key = std::str::from_utf8(&record[..equals]).map_err(invalid)?;
        if key.is_empty() || key.contains(['\n', '\0']) {
            return Err(invalid("invalid PAX key"));
        }
        result.push((key, &record[equals + 1..]));
        data = &data[length..];
    }
    Ok(result)
}

fn append(output: &mut Vec<u8>, key: &str, value: &[u8]) {
    let base = key.len() + value.len() + 3;
    let mut length = base + 1;
    loop {
        let next = base + length.to_string().len();
        if next == length {
            break;
        }
        length = next;
    }
    output.extend(format!("{length} {key}=").as_bytes());
    output.extend(value);
    output.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn length_delimited_binary_and_malformed_records() {
        let mut bytes = Vec::new();
        append(&mut bytes, "SCHILY.xattr.user.binary", &[0, 10, 255]);
        assert_eq!(records(&bytes).unwrap()[0].1, [0, 10, 255]);
        for bad in [b"0 x=y\n".as_slice(), b"999 x=y\n", b"7 x=y!", b"x"] {
            assert!(records(bad).is_err());
        }
    }
}
