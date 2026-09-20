//! Parser for GDB's machine interface. Lists preserve repeated named entries.

use anyhow::{Result, bail, ensure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    String(String),
    Tuple(Vec<(String, Value)>),
    List(Vec<Value>),
}

impl Value {
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Self::Tuple(fields) => fields.iter().find(|(key, _)| key == name).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn string(&self) -> Result<&str> {
        match self {
            Self::String(s) => Ok(s),
            _ => bail!("expected an MI string, got {self:?}"),
        }
    }

    pub fn list(&self) -> Result<&[Value]> {
        match self {
            Self::List(values) => Ok(values),
            _ => bail!("expected an MI list, got {self:?}"),
        }
    }

    pub fn field(&self, name: &str) -> Result<&Value> {
        self.get(name)
            .ok_or_else(|| anyhow::anyhow!("missing MI field {name}: {self:?}"))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Record {
    Result {
        token: Option<u64>,
        class: String,
        data: Value,
    },
    Async {
        kind: u8,
        class: String,
        data: Value,
    },
    Stream {
        kind: u8,
        text: String,
    },
    Prompt,
}

pub fn parse(line: &str) -> Result<Record> {
    let line = line.trim_end_matches(['\r', '\n', ' ']);
    if line == "(gdb)" || line.is_empty() {
        return Ok(Record::Prompt);
    }
    let mut p = Parser {
        bytes: line.as_bytes(),
        pos: 0,
    };
    let start = p.pos;
    while p.peek().is_some_and(|b| b.is_ascii_digit()) {
        p.pos += 1;
    }
    let token = if p.pos > start {
        Some(line[start..p.pos].parse()?)
    } else {
        None
    };
    let kind = p.next()?;
    let record = match kind {
        b'~' | b'@' | b'&' => Record::Stream {
            kind,
            text: p.string()?,
        },
        b'^' | b'*' | b'+' | b'=' => {
            let class = p.identifier()?;
            let mut fields = Vec::new();
            while p.peek() == Some(b',') {
                p.pos += 1;
                fields.push(p.field(0)?);
            }
            let data = Value::Tuple(fields);
            if kind == b'^' {
                Record::Result { token, class, data }
            } else {
                Record::Async { kind, class, data }
            }
        }
        _ => bail!("invalid MI record: {line:?}"),
    };
    ensure!(
        p.pos == p.bytes.len(),
        "trailing data in MI record: {line:?}"
    );
    Ok(record)
}

/// Quote an argument for MI and for GDB console filename arguments.
pub fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for b in s.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            32..=126 => out.push(char::from(b)),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push('"');
    out
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn next(&mut self) -> Result<u8> {
        let b = self
            .peek()
            .ok_or_else(|| anyhow::anyhow!("truncated MI record"))?;
        self.pos += 1;
        Ok(b)
    }
    fn expect(&mut self, expected: u8) -> Result<()> {
        ensure!(
            self.next()? == expected,
            "expected {:?} in MI record",
            char::from(expected)
        );
        Ok(())
    }
    fn identifier(&mut self) -> Result<String> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            self.pos += 1;
        }
        ensure!(self.pos > start, "missing MI identifier");
        Ok(std::str::from_utf8(&self.bytes[start..self.pos])?.into())
    }
    fn string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            match self.next()? {
                b'"' => break,
                b'\\' => {
                    let b = self.next()?;
                    match b {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'v' => out.push(11),
                        b'a' => out.push(7),
                        b'\\' | b'"' | b'\'' => out.push(b),
                        b'0'..=b'7' => {
                            let mut n = u16::from(b - b'0');
                            for _ in 0..2 {
                                if let Some(next @ b'0'..=b'7') = self.peek() {
                                    self.pos += 1;
                                    n = n * 8 + u16::from(next - b'0');
                                } else {
                                    break;
                                }
                            }
                            out.push(u8::try_from(n)?);
                        }
                        _ => bail!("unsupported MI escape: {}", char::from(b)),
                    }
                }
                b => out.push(b),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
    fn field(&mut self, depth: usize) -> Result<(String, Value)> {
        let key = self.identifier()?;
        self.expect(b'=')?;
        Ok((key, self.value(depth + 1)?))
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        ensure!(depth < 128, "MI nesting limit exceeded");
        match self.peek() {
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'{') => {
                self.pos += 1;
                let mut fields = Vec::new();
                if self.peek() != Some(b'}') {
                    loop {
                        fields.push(self.field(depth)?);
                        if self.peek() != Some(b',') {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                self.expect(b'}')?;
                Ok(Value::Tuple(fields))
            }
            Some(b'[') => {
                self.pos += 1;
                let mut values = Vec::new();
                if self.peek() != Some(b']') {
                    loop {
                        values.push(match self.peek() {
                            Some(b'"' | b'{' | b'[') => self.value(depth + 1)?,
                            _ => Value::Tuple(vec![self.field(depth)?]),
                        });
                        if self.peek() != Some(b',') {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                self.expect(b']')?;
                Ok(Value::List(values))
            }
            _ => bail!("invalid MI value at byte {}", self.pos),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_and_repeated_results() {
        let Record::Result { token, data, .. } = parse("42^done,stack=[frame={level=\"0\",addr=\"0xffff800000001000\"},frame={level=\"1\"}],empty=[],value=\"a,b={c}\"").unwrap() else { panic!() };
        assert_eq!(token, Some(42));
        assert_eq!(data.field("stack").unwrap().list().unwrap().len(), 2);
        assert_eq!(data.field("value").unwrap().string().unwrap(), "a,b={c}");
    }

    #[test]
    fn strings_round_trip() {
        let text = "path with spaces/\"quotes\"/\\/λ\n\t\0";
        assert_eq!(
            parse(&format!("~{}", quote(text))).unwrap(),
            Record::Stream {
                kind: b'~',
                text: text.into()
            }
        );
        assert!(parse("1^done,value=\"unterminated").is_err());
        assert!(parse("1^done,items=[{a=\"b\"}").is_err());
        assert!(parse("1^done trailing").is_err());
    }

    #[test]
    fn asynchronous_stop() {
        assert!(matches!(
            parse("*stopped,reason=\"breakpoint-hit\",bkptno=\"2\",thread-id=\"3\"\r\n").unwrap(),
            Record::Async { kind: b'*', .. }
        ));
        assert_eq!(parse("(gdb) ").unwrap(), Record::Prompt);
    }
}
