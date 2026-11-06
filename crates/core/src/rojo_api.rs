//! Identify a local Rojo server from its /api/rojo endpoint. Rojo 7.7 answers in MessagePack;
//! earlier versions answer in JSON.

use std::time::Duration;

use serde_json::{Map, Number, Value};

use crate::localnet;

/// Deepest nesting accepted, so hostile input cannot overflow the stack.
const MAX_DEPTH: usize = 64;

/// Decode one MessagePack value made of nil, booleans, numbers, strings, binary (as an array of
/// bytes), arrays, and maps. Errors on anything else, truncation, or trailing data.
pub fn unpack_msgpack(data: &[u8]) -> Result<Value, String> {
    let mut reader = Reader { data, position: 0 };
    let value = reader.value(0)?;
    if reader.position != data.len() {
        return Err("Trailing MessagePack data".into());
    }
    Ok(value)
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], String> {
        let end = self.position.checked_add(size).filter(|end| *end <= self.data.len());
        let end = end.ok_or("Truncated MessagePack data")?;
        let chunk = &self.data[self.position..end];
        self.position = end;
        Ok(chunk)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut bytes = [0; N];
        bytes.copy_from_slice(self.take(N)?);
        Ok(bytes)
    }

    /// A big-endian length of 1, 2, or 4 bytes.
    fn length(&mut self, bytes: usize) -> Result<usize, String> {
        Ok(self.take(bytes)?.iter().fold(0, |length, byte| length << 8 | usize::from(*byte)))
    }

    fn string(&mut self, length: usize) -> Result<Value, String> {
        let text = std::str::from_utf8(self.take(length)?).map_err(|_| "Invalid UTF-8 in MessagePack string")?;
        Ok(Value::String(text.to_string()))
    }

    fn list(&mut self, length: usize, depth: usize) -> Result<Value, String> {
        // Lengths come from the input, so only reserve what the remaining bytes could hold.
        let mut items = Vec::with_capacity(length.min(self.data.len() - self.position));
        for _ in 0..length {
            items.push(self.value(depth + 1)?);
        }
        Ok(Value::Array(items))
    }

    fn map(&mut self, length: usize, depth: usize) -> Result<Value, String> {
        let mut map = Map::new();
        for _ in 0..length {
            let key = match self.value(depth + 1)? {
                Value::String(key) => key,
                other => other.to_string(),
            };
            map.insert(key, self.value(depth + 1)?);
        }
        Ok(Value::Object(map))
    }

    fn float(number: f64) -> Value {
        Number::from_f64(number).map_or(Value::Null, Value::Number)
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("MessagePack data is nested too deeply".into());
        }
        let tag = self.take(1)?[0];
        Ok(match tag {
            0x00..=0x7f => Value::from(tag),
            0xe0..=0xff => Value::from(tag as i8),
            0x80..=0x8f => self.map(usize::from(tag & 0x0f), depth)?,
            0x90..=0x9f => self.list(usize::from(tag & 0x0f), depth)?,
            0xa0..=0xbf => self.string(usize::from(tag & 0x1f))?,
            0xc0 => Value::Null,
            0xc2 => Value::Bool(false),
            0xc3 => Value::Bool(true),
            0xc4..=0xc6 => {
                let length = self.length(1 << (tag - 0xc4))?;
                Value::from(self.take(length)?.to_vec())
            }
            0xd9..=0xdb => {
                let length = self.length(1 << (tag - 0xd9))?;
                self.string(length)?
            }
            0xdc | 0xdd => {
                let length = self.length(if tag == 0xdc { 2 } else { 4 })?;
                self.list(length, depth)?
            }
            0xde | 0xdf => {
                let length = self.length(if tag == 0xde { 2 } else { 4 })?;
                self.map(length, depth)?
            }
            0xca => Self::float(f64::from(f32::from_be_bytes(self.array()?))),
            0xcb => Self::float(f64::from_be_bytes(self.array()?)),
            0xcc => Value::from(self.take(1)?[0]),
            0xcd => Value::from(u16::from_be_bytes(self.array()?)),
            0xce => Value::from(u32::from_be_bytes(self.array()?)),
            0xcf => Value::from(u64::from_be_bytes(self.array()?)),
            0xd0 => Value::from(self.take(1)?[0] as i8),
            0xd1 => Value::from(i16::from_be_bytes(self.array()?)),
            0xd2 => Value::from(i32::from_be_bytes(self.array()?)),
            0xd3 => Value::from(i64::from_be_bytes(self.array()?)),
            _ => return Err(format!("Unsupported MessagePack type 0x{tag:02x}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub server_version: String,
    pub session_id: String,
    pub project_name: String,
    pub protocol_version: i64,
}

/// Rojo's server info when a Rojo server answers on this local port, otherwise None.
pub fn server_info(port: u16, timeout: Duration) -> Option<ServerInfo> {
    let response = localnet::request(port, "GET", "/api/rojo", None, &[], timeout, Some(65536)).ok()?;
    if response.status != 200 {
        return None;
    }
    let info = if response.content_type.contains("msgpack") {
        unpack_msgpack(&response.body).ok()?
    } else {
        serde_json::from_slice(&response.body).ok()?
    };
    parse_server_info(&info)
}

fn parse_server_info(info: &Value) -> Option<ServerInfo> {
    let text = |key: &str| info.get(key)?.as_str().map(str::to_string);
    Some(ServerInfo {
        server_version: text("serverVersion")?,
        session_id: text("sessionId")?,
        project_name: text("projectName")?,
        protocol_version: info.get("protocolVersion")?.as_i64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// GET /api/rojo from a Rojo 7.7.0 server, with its name and IDs replaced.
    const ROJO_77_INFO: &str = concat!(
        "89a973657373696f6e4964d92430303030303030302d303030302d343030302d383030302d303030303030303030303031",
        "ad73657276657256657273696f6ea5372e372e30af70726f746f636f6c56657273696f6e05ab70726f6a6563744e616d65",
        "a85465737447616d65b06578706563746564506c616365496473c0b2756e6578706563746564506c616365496473c0a667",
        "616d654964c0a7706c6163654964c0ae726f6f74496e7374616e63654964d9203031323334353637383961626364656630",
        "313233343536373839616263646566",
    );

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn rojo_77_server_info_decodes() {
        let info = unpack_msgpack(&hex(ROJO_77_INFO)).unwrap();
        assert_eq!(info["placeId"], Value::Null);
        assert_eq!(
            parse_server_info(&info),
            Some(ServerInfo {
                server_version: "7.7.0".into(),
                session_id: "00000000-0000-4000-8000-000000000001".into(),
                project_name: "TestGame".into(),
                protocol_version: 5,
            })
        );
        assert_eq!(parse_server_info(&json!({"serverVersion": "7.4.0", "sessionId": "s", "projectName": "P"})), None);
    }

    #[test]
    fn other_msgpack_types_and_malformed_input() {
        assert_eq!(unpack_msgpack(&[0x93, 0xff, 0xcd, 0x01, 0x00, 0xc3]).unwrap(), json!([-1, 256, true]));
        assert_eq!(unpack_msgpack(&[0x81, 0x01, 0xc4, 0x02, 0x07, 0x08]).unwrap(), json!({"1": [7, 8]}));
        assert_eq!(unpack_msgpack(&[0xcb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0]).unwrap(), json!(1.5));
        for bad in [&b"\xa5abc"[..], b"\x01\x02", b"\xc1", b"\xd4\x00\x00", b"\xa1\xff"] {
            assert!(unpack_msgpack(bad).is_err(), "{bad:?}");
        }
        let deep = [vec![0x91; 100], vec![0xc0]].concat(); // [[[...[nil]...]]]
        assert!(unpack_msgpack(&deep).is_err());
    }
}
