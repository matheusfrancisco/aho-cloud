//! Lines of object keys in JSON text (serde_json keeps no positions).

use crate::line_of;

/// 1-based line of the key at `path` (object keys from the root), if present.
pub(crate) fn key_line(text: &str, path: &[&str]) -> Option<usize> {
    let mut s = Scan { b: text.as_bytes(), i: 0 };
    s.value(path).ok().flatten().map(|off| line_of(text, off))
}

struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

type Found = Result<Option<usize>, ()>;

impl Scan<'_> {
    fn ws(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    /// Consumes one value; returns the offset of the key at `path` if it lies inside.
    fn value(&mut self, path: &[&str]) -> Found {
        self.ws();
        match self.peek().ok_or(())? {
            b'{' => self.object(path),
            b'[' => {
                self.i += 1;
                loop {
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        return Ok(None);
                    }
                    self.value(&[])?;
                    self.ws();
                    match self.peek().ok_or(())? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Ok(None);
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'"' => self.string().map(|_| None),
            _ => {
                while self.peek().is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace()) {
                    self.i += 1;
                }
                Ok(None)
            }
        }
    }

    fn object(&mut self, path: &[&str]) -> Found {
        self.i += 1;
        loop {
            self.ws();
            match self.peek().ok_or(())? {
                b'}' => {
                    self.i += 1;
                    return Ok(None);
                }
                b'"' => {}
                _ => return Err(()),
            }
            let at = self.i;
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(());
            }
            self.i += 1;
            match path.split_first() {
                Some((want, [])) if key == *want => return Ok(Some(at)),
                Some((want, rest)) if key == *want => return self.value(rest),
                _ => {
                    self.value(&[])?;
                }
            }
            self.ws();
            match self.peek().ok_or(())? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Ok(None);
                }
                _ => return Err(()),
            }
        }
    }

    /// A JSON string starting at the current `"`, decoded.
    fn string(&mut self) -> Result<String, ()> {
        let start = self.i;
        self.i += 1;
        loop {
            match self.peek().ok_or(())? {
                b'\\' => self.i += 2,
                b'"' => {
                    self.i += 1;
                    let raw = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| ())?;
                    return serde_json::from_str(raw).map_err(|_| ());
                }
                _ => self.i += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_keys_and_skips_lookalikes() {
        let text = "{\n  \"description\": \"a \\\"start\\\" here\",\n  \"x\": [{\"start\": 1}],\n  \"scripts\": {\n    \"build\": \"b\",\n    \"start\": \"node a.js\"\n  }\n}\n";
        assert_eq!(key_line(text, &["scripts", "start"]), Some(6));
        assert_eq!(key_line(text, &["scripts"]), Some(4));
        assert_eq!(key_line(text, &["start"]), None);
        assert_eq!(key_line("{\"a\": ", &["a", "b"]), None);
    }
}
