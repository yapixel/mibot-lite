use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MiboxError {
    #[error("不是 SQLite 数据库")]
    NotSqlite,
    #[error("SQLite 页大小异常")]
    InvalidPageSize,
    #[error("页号 {0} 超出文件")]
    PageOutOfRange(u32),
    #[error("SQLite B 树有环")]
    BTreeCycle,
    #[error("SQLite 单元数异常")]
    InvalidCellCount,
    #[error("SQLite 单元偏移异常")]
    InvalidCellOffset,
    #[error("不是表 B 树页（类型 0x{0:02x}）")]
    InvalidPageKind(u8),
    #[error("SQLite 负载越界")]
    PayloadOutOfBounds,
    #[error("SQLite 记录头越界")]
    RecordHeaderOutOfBounds,
    #[error("SQLite 记录越界")]
    RecordOutOfBounds,
    #[error("表的根页号异常")]
    InvalidTableRoot,
    #[error("没有 {0} 表")]
    TableNotFound(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }
}

struct SqliteFile<'a> {
    data: &'a [u8],
    page_size: usize,
    usable: usize,
}

impl<'a> SqliteFile<'a> {
    fn parse(data: &'a [u8]) -> Result<Self, MiboxError> {
        if data.len() < 100 || &data[..16] != b"SQLite format 3\0" {
            return Err(MiboxError::NotSqlite);
        }
        let raw_size = u16::from_be_bytes([data[16], data[17]]) as usize;
        let page_size = if raw_size == 1 { 65536 } else { raw_size };
        if page_size < 512 || !data.len().is_multiple_of(page_size) {
            return Err(MiboxError::InvalidPageSize);
        }
        let reserved = data[20] as usize;
        let usable = page_size
            .checked_sub(reserved)
            .filter(|&u| u >= 40)
            .ok_or(MiboxError::InvalidPageSize)?;
        Ok(Self {
            data,
            page_size,
            usable,
        })
    }

    fn page(&self, n: u32) -> Result<&'a [u8], MiboxError> {
        if n == 0 {
            return Err(MiboxError::PageOutOfRange(n));
        }
        let start = (n as usize - 1).saturating_mul(self.page_size);
        let end = start.checked_add(self.page_size);
        match end {
            Some(e) if e <= self.data.len() => Ok(&self.data[start..e]),
            _ => Err(MiboxError::PageOutOfRange(n)),
        }
    }

    fn rows(&self, root: u32) -> Result<Vec<Vec<Value>>, MiboxError> {
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        self.walk(root, &mut visited, &mut out)?;
        Ok(out)
    }

    fn walk(
        &self,
        n: u32,
        visited: &mut HashSet<u32>,
        out: &mut Vec<Vec<Value>>,
    ) -> Result<(), MiboxError> {
        if !visited.insert(n) {
            return Err(MiboxError::BTreeCycle);
        }
        let page = self.page(n)?;
        let header = if n == 1 { 100 } else { 0 };
        if header + 8 > page.len() {
            return Err(MiboxError::InvalidCellCount);
        }
        let kind = page[header];
        let cells = u16::from_be_bytes([page[header + 3], page[header + 4]]) as usize;
        let pointers = if kind == 0x05 {
            header + 12
        } else {
            header + 8
        };
        if pointers + 2 * cells > page.len() {
            return Err(MiboxError::InvalidCellCount);
        }
        for index in 0..cells {
            let ptr_off = pointers + 2 * index;
            let offset = u16::from_be_bytes([page[ptr_off], page[ptr_off + 1]]) as usize;
            if offset + 4 > page.len() {
                return Err(MiboxError::InvalidCellOffset);
            }
            match kind {
                0x05 => {
                    let child = u32::from_be_bytes([
                        page[offset],
                        page[offset + 1],
                        page[offset + 2],
                        page[offset + 3],
                    ]);
                    self.walk(child, visited, out)?;
                }
                0x0D => {
                    let (size, used) = varint(&page[offset..]);
                    let (_, rowid_len) = varint(&page[offset + used..]);
                    let payload = self.payload(page, offset + used + rowid_len, size)?;
                    out.push(record(&payload)?);
                }
                _ => return Err(MiboxError::InvalidPageKind(kind)),
            }
        }
        if kind == 0x05 {
            let right = u32::from_be_bytes([
                page[header + 8],
                page[header + 9],
                page[header + 10],
                page[header + 11],
            ]);
            self.walk(right, visited, out)?;
        }
        Ok(())
    }

    fn payload(
        &self,
        page: &[u8],
        start: usize,
        size_field: u64,
    ) -> Result<Vec<u8>, MiboxError> {
        if size_field > self.data.len() as u64 {
            return Err(MiboxError::PayloadOutOfBounds);
        }
        let size = size_field as usize;
        let max_local = self.usable - 35;
        if size <= max_local {
            let end = start
                .checked_add(size)
                .ok_or(MiboxError::PayloadOutOfBounds)?;
            if end > page.len() {
                return Err(MiboxError::PayloadOutOfBounds);
            }
            return Ok(page[start..end].to_vec());
        }
        let min_local = (self.usable - 12) * 32 / 255 - 23;
        let mut local = min_local + (size - min_local) % (self.usable - 4);
        if local > max_local {
            local = min_local;
        }
        if start + local + 4 > page.len() {
            return Err(MiboxError::PayloadOutOfBounds);
        }
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&page[start..start + local]);
        let mut next = u32::from_be_bytes([
            page[start + local],
            page[start + local + 1],
            page[start + local + 2],
            page[start + local + 3],
        ]);
        while out.len() < size {
            let overflow = self.page(next)?;
            let chunk = (size - out.len()).min(self.usable - 4);
            if 4 + chunk > overflow.len() {
                return Err(MiboxError::PayloadOutOfBounds);
            }
            out.extend_from_slice(&overflow[4..4 + chunk]);
            next = u32::from_be_bytes([overflow[0], overflow[1], overflow[2], overflow[3]]);
        }
        Ok(out)
    }
}

fn varint(data: &[u8]) -> (u64, usize) {
    let mut value: u64 = 0;
    for (index, &byte) in data.iter().take(9).enumerate() {
        if index == 8 {
            return ((value << 8) | u64::from(byte), 9);
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return (value, index + 1);
        }
    }
    (value, data.len().min(9))
}

fn record(payload: &[u8]) -> Result<Vec<Value>, MiboxError> {
    let (header_size_u64, used) = varint(payload);
    if header_size_u64 < used as u64 || header_size_u64 > payload.len() as u64 {
        return Err(MiboxError::RecordHeaderOutOfBounds);
    }
    let header_size = header_size_u64 as usize;
    let mut types = Vec::new();
    let mut position = used;
    while position < header_size {
        let (value, n) = varint(&payload[position..header_size]);
        if n == 0 {
            return Err(MiboxError::RecordHeaderOutOfBounds);
        }
        types.push(value);
        position += n;
    }

    let mut body = &payload[header_size..];
    let mut values = Vec::with_capacity(types.len());
    for kind in types {
        let size: usize = if kind >= 12 && kind.is_multiple_of(2) {
            ((kind - 12) / 2) as usize
        } else if kind >= 13 {
            ((kind - 13) / 2) as usize
        } else if (1..=4).contains(&kind) {
            kind as usize
        } else if kind == 5 {
            6
        } else if kind == 6 || kind == 7 {
            8
        } else {
            0
        };

        if size > body.len() {
            return Err(MiboxError::RecordOutOfBounds);
        }
        let (field, rest) = body.split_at(size);
        body = rest;

        let val = match kind {
            0 => Value::Null,
            8 => Value::Int(0),
            9 => Value::Int(1),
            7 => {
                let bytes: [u8; 8] = field.try_into().map_err(|_| MiboxError::RecordOutOfBounds)?;
                Value::Float(f64::from_be_bytes(bytes))
            }
            1..=6 => {
                let mut number: i64 = 0;
                for &b in field {
                    number = (number << 8) | i64::from(b);
                }
                let shift = 64 - 8 * size;
                Value::Int((number << shift) >> shift)
            }
            k if k >= 13 && k % 2 == 1 => {
                Value::Text(String::from_utf8_lossy(field).into_owned())
            }
            _ => Value::Blob(field.to_vec()),
        };
        values.push(val);
    }
    Ok(values)
}

fn read_table(data: &[u8], name: &str) -> Result<Vec<Vec<Value>>, MiboxError> {
    let file = SqliteFile::parse(data)?;
    let schema = file.rows(1)?;
    for row in schema {
        if row.len() >= 4 && row[0].as_str() == Some("table") && row[1].as_str() == Some(name) {
            let root = row[3].as_i64().filter(|&r| r > 0).ok_or(MiboxError::InvalidTableRoot)?;
            let root_u32 = u32::try_from(root).map_err(|_| MiboxError::InvalidTableRoot)?;
            return file.rows(root_u32);
        }
    }
    Err(MiboxError::TableNotFound(name.to_owned()))
}

/// Parses MiBox's `assets/alias/alias.db` (`aliases` table: `original -> final`).
pub fn aliases(data: &[u8]) -> Result<HashMap<String, String>, MiboxError> {
    let rows = read_table(data, "aliases")?;
    let mut map = HashMap::with_capacity(rows.len());
    for row in rows {
        if row.len() < 2 {
            continue;
        }
        if let (Some(original), Some(final_cmd)) = (row[0].as_str(), row[1].as_str())
            && !original.is_empty()
            && !final_cmd.is_empty()
        {
            map.insert(original.to_owned(), final_cmd.to_owned());
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rejects_impossible_sqlite_lengths() {
        let overflowed = vec![0xff; 9];
        assert!(record(&overflowed).is_err());

        let buf = vec![0u8; 512];
        let file = SqliteFile {
            data: &buf,
            page_size: 512,
            usable: 512,
        };
        assert!(file.payload(&buf, 0, u64::MAX).is_err());
    }

    #[test]
    fn parses_real_mibox_alias_db() {
        let db_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/alias.db");
        let data = std::fs::read(&db_path).expect("read testdata/alias.db");
        let parsed = aliases(&data).expect("parse aliases");
        assert_eq!(parsed.len(), 403);
        assert_eq!(parsed.get("测速").map(String::as_str), Some("speedtest 12345"));
        assert_eq!(parsed.get("p").map(String::as_str), Some("ping"));
        let expected_long = format!("sum run {}", "x".repeat(5000));
        assert_eq!(parsed.get("长").map(String::as_str), Some(expected_long.as_str()));
        let expected_last = format!("ping {}", "y".repeat(399 % 50));
        assert_eq!(parsed.get("a399").map(String::as_str), Some(expected_last.as_str()));
    }

    #[test]
    fn rejects_non_sqlite() {
        assert!(aliases(b"not a database").is_err());
    }

    #[test]
    fn varint_cases() {
        assert_eq!(varint(&[0x05]), (5, 1));
        assert_eq!(varint(&[0x81, 0x00]), (128, 2));
        assert_eq!(varint(&[0x82, 0x80, 0x01]), (0x8001, 3));
    }
}
