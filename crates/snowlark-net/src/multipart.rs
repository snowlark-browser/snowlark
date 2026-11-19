// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parsing a `multipart/form-data` body (the Body mixin's `formData()`, Fetch §6.2): RFC 7578 on
//! RFC 2046 §5.1.1's grammar. A body that does not follow it is a failure, which `formData()`
//! turns into a TypeError.
//!
//! ```text
//! body           := [preamble CRLF] dash-boundary padding CRLF part
//!                   *(CRLF dash-boundary padding CRLF part)
//!                   CRLF dash-boundary "--" padding [CRLF epilogue]
//! dash-boundary  := "--" boundary
//! padding        := *(SP / HTAB)
//! part           := *(header-field CRLF) CRLF octets
//! ```

/// One part of the body: its `Content-Disposition` name and filename, its `Content-Type`, and
/// its octets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn skip_padding(input: &[u8], mut pos: usize) -> usize {
    while matches!(input.get(pos), Some(b' ' | b'\t')) {
        pos += 1;
    }
    pos
}

/// Parses `input` with `boundary` (the MIME type's `boundary` parameter). `None` is failure.
pub fn parse(input: &[u8], boundary: &str) -> Option<Vec<Part>> {
    if boundary.is_empty() {
        return None;
    }
    let dash_boundary = format!("--{boundary}");
    let delimiter = format!("\r\n{dash_boundary}");
    // The first delimiter: a dash-boundary at the start, or one after a preamble.
    let mut pos = if input.starts_with(dash_boundary.as_bytes()) {
        dash_boundary.len()
    } else {
        find(input, delimiter.as_bytes())? + delimiter.len()
    };
    let mut parts = Vec::new();
    loop {
        // After a delimiter: "--" closes the body; otherwise padding and CRLF start a part.
        if input[pos..].starts_with(b"--") {
            pos = skip_padding(input, pos + 2);
            // The epilogue after the CRLF is ignored.
            return (pos == input.len() || input[pos..].starts_with(b"\r\n")).then_some(parts);
        }
        pos = skip_padding(input, pos);
        if !input[pos..].starts_with(b"\r\n") {
            return None;
        }
        pos += 2;
        let mut name = None;
        let mut filename = None;
        let mut content_type = None;
        // Header fields up to the empty line.
        loop {
            let end = find(&input[pos..], b"\r\n")?;
            let line = &input[pos..pos + end];
            pos += end + 2;
            if line.is_empty() {
                break;
            }
            let colon = line.iter().position(|&b| b == b':')?;
            let field = String::from_utf8_lossy(&line[..colon]);
            let value = String::from_utf8_lossy(&line[colon + 1..]);
            let value = value.trim_matches(|c| c == ' ' || c == '\t');
            if field.eq_ignore_ascii_case("content-disposition") {
                let mut params = value.split(';');
                if !params
                    .next()
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("form-data"))
                {
                    return None;
                }
                for p in params {
                    let Some((k, v)) = p.split_once('=') else {
                        continue;
                    };
                    let v = v.trim();
                    let v = v
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .unwrap_or(v);
                    match k.trim().to_ascii_lowercase().as_str() {
                        "name" => name = Some(v.to_owned()),
                        "filename" => filename = Some(v.to_owned()),
                        _ => {}
                    }
                }
            } else if field.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.to_owned());
            }
        }
        // The octets run to the next delimiter.
        let end = find(&input[pos..], delimiter.as_bytes())?;
        let body = input[pos..pos + end].to_vec();
        pos += end + delimiter.len();
        parts.push(Part {
            name: name?,
            filename,
            content_type,
            body,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(input: &str, boundary: &str) -> Option<Vec<(String, String)>> {
        parse(input.as_bytes(), boundary).map(|ps| {
            ps.into_iter()
                .map(|p| (p.name, String::from_utf8(p.body).unwrap()))
                .collect()
        })
    }

    const PART: &str = "Content-Disposition: form-data; name=\"username\"\r\n\r\nfoo";

    #[test]
    fn well_formed_bodies() {
        let one = vec![("username".to_owned(), "foo".to_owned())];
        for body in [
            format!("--b\r\n{PART}\r\n--b--\r\n"),
            format!("--b \t\r\n{PART}\r\n--b--\r\n"),
            format!("--b\r\n{PART}\r\n--b--"),
            format!("--b\r\n{PART}\r\n--b--\r\nepilogue --b"),
            format!("preamble\r\n--b\r\n{PART}\r\n--b--"),
        ] {
            assert_eq!(names(&body, "b").as_ref(), Some(&one), "{body:?}");
        }
        let two = format!(
            "--b\r\n{PART}\r\n--b\t \r\nContent-Disposition: form-data; name=account\r\n\r\ngoo\r\n--b--"
        );
        assert_eq!(
            names(&two, "b").unwrap(),
            [
                ("username".into(), "foo".into()),
                ("account".into(), "goo".into())
            ]
        );
        let file = parse(
            b"--b\r\nContent-Disposition: form-data; name=f; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nx\r\ny\r\n--b--",
            "b",
        )
        .unwrap();
        assert_eq!(file[0].filename.as_deref(), Some("a.txt"));
        assert_eq!(file[0].content_type.as_deref(), Some("text/plain"));
        assert_eq!(file[0].body, b"x\r\ny");
        assert_eq!(names("--b--", "b"), Some(vec![]));
    }

    #[test]
    fn malformed_bodies_fail() {
        for (body, boundary) in [
            ("--boundary--boundary--\r\n", "boundary"),
            ("--boundary\n--boundary--\r\n", "boundary"),
            ("--boundary\r--boundary--\r\n", "boundary"),
            ("--boundary\r\n\r\n\r\n--boundary--\r\n", "boundary"),
            ("--boundary\n_--boundary--\r\n", "boundary"),
            ("--boundary- ---boundary--\r\n", "boundary"),
            ("--boundary--\r\n", "boundary--"),
            ("no delimiter", "b"),
            (
                "--b\r\nContent-Disposition: form-data; name=a\r\n\r\nunterminated",
                "b",
            ),
            (
                "--b\r\nContent-Disposition: attachment; name=a\r\n\r\nx\r\n--b--",
                "b",
            ),
            ("--b\r\nno colon\r\n\r\nx\r\n--b--", "b"),
        ] {
            assert_eq!(names(body, boundary), None, "{body:?}");
        }
    }
}
