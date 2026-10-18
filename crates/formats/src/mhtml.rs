//! MHTML (RFC 2557, MIME HTML): a web page and the pictures it shows in one MIME message, the
//! "Single File Web Page" `.mht`/`.mhtml`. Word opens it as a web page; many programs (wiki and
//! report exports) write it under a `.doc` name for Word to open (#2853).
//!
//! The message's parts are split at the multipart boundary and decoded (base64,
//! quoted-printable, 7bit/8bit/binary). The first `text/html` part (or the one the `start`
//! parameter names) is imported as HTML in its charset, and its pictures load from the other
//! parts by `Content-Location` (also relative to the page's own) or `cid:` `Content-ID`.

use std::sync::Arc;

use wordcraft_doc::Document;

/// Parts read at most: a page and its pictures are a few hundred at the very most.
const MAX_PARTS: usize = 10_000;
/// Header lines of one part looked at (folded lines count once each).
const MAX_HEADER_LINES: usize = 200;
/// How far into the file the message headers are looked for.
const SNIFF_BYTES: usize = 8192;

/// One decoded part of the message.
#[derive(Debug)]
struct Part {
    /// `type/subtype`, lowercase.
    mime: String,
    charset: Option<String>,
    location: Option<String>,
    id: Option<String>,
    body: Vec<u8>,
}

/// Does `bytes` start like an MHTML message: headers with `MIME-Version` and a
/// `multipart/related` (or `text/html`) content type before the first blank line?
pub fn is_mhtml(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(bytes.get(..bytes.len().min(SNIFF_BYTES)).unwrap_or(bytes)).to_ascii_lowercase();
    let head = head.trim_start_matches(['\u{FEFF}', ' ', '\t', '\r', '\n']);
    let end = [head.find("\r\n\r\n"), head.find("\n\n")].into_iter().flatten().min().unwrap_or(head.len());
    let headers = head.get(..end).unwrap_or(head);
    // Every line of a header section is a header or its continuation, never markup.
    let all_headers =
        headers.lines().all(|l| l.starts_with([' ', '\t']) || l.split_once(':').is_some_and(|(k, _)| !k.is_empty() && !k.contains([' ', '<'])));
    all_headers
        && headers.lines().any(|l| l.starts_with("mime-version:"))
        && headers.lines().any(|l| l.starts_with("content-type:") && (l.contains("multipart/related") || l.contains("text/html")))
}

/// Import an MHTML message onto `page` (its size and margins: web pages give none).
pub fn import_on(bytes: &[u8], page: &wordcraft_doc::SectionProps) -> Result<Document, String> {
    let (headers, body) = split_headers(bytes);
    let content_type = header(&headers, "content-type").unwrap_or_default();
    let mut parts = Vec::new();
    match param(&content_type, "boundary") {
        Some(boundary) if mime_of(&content_type).starts_with("multipart/") => {
            for raw in split_parts(body, boundary.as_bytes()).into_iter().take(MAX_PARTS) {
                let (h, b) = split_headers(raw);
                parts.push(part(&h, b));
            }
        }
        _ => parts.push(part(&headers, body)),
    }
    let start = param(&content_type, "start").map(|s| s.trim_matches(['<', '>']).to_string());
    let root = start
        .and_then(|cid| parts.iter().position(|p| p.id.as_deref() == Some(cid.as_str())))
        .or_else(|| parts.iter().position(|p| p.mime == "text/html" || p.mime == "application/xhtml+xml"))
        .ok_or_else(|| "MHTML: no web page in the message".to_string())?;
    let page_part = parts.get(root).ok_or_else(|| "MHTML: no web page in the message".to_string())?;
    let html = decode_text(&page_part.body, page_part.charset.as_deref());
    let base = page_part.location.clone().unwrap_or_default();
    let loader = |src: &str| picture(&parts, &base, src);
    let mut doc = crate::html::import_on(&html, &loader, page);
    doc.ensure_nonempty();
    Ok(doc)
}

/// The part a page's picture `src` names: `cid:` by its Content-ID, else by its
/// Content-Location, as given or relative to the page's (`base`), or by its file name's path.
fn picture(parts: &[Part], base: &str, src: &str) -> Option<Arc<Vec<u8>>> {
    let src = src.trim();
    if let Some(cid) = src.strip_prefix("cid:") {
        return parts.iter().find(|p| p.id.as_deref() == Some(cid)).map(|p| Arc::new(p.body.clone()));
    }
    let rel = src.trim_start_matches("./");
    let joined = base.rfind(['/', '\\']).and_then(|i| base.get(..=i)).map(|dir| format!("{dir}{rel}"));
    let named = |p: &&Part| {
        let Some(loc) = p.location.as_deref() else { return false };
        loc == src
            || joined.as_deref().is_some_and(|j| loc.eq_ignore_ascii_case(j))
            || (!rel.is_empty() && !rel.contains(':') && (loc.ends_with(&format!("/{rel}")) || loc.ends_with(&format!("\\{rel}"))))
    };
    parts.iter().filter(|p| !p.mime.starts_with("text/")).find(named).map(|p| Arc::new(p.body.clone()))
}

/// A part from its header lines and raw body.
fn part(headers: &[(String, String)], body: &[u8]) -> Part {
    let content_type = header(headers, "content-type").unwrap_or_else(|| "text/plain".to_string());
    let encoding = header(headers, "content-transfer-encoding").unwrap_or_default().to_ascii_lowercase();
    let body = match encoding.trim() {
        "base64" => crate::model::base64_decode(&String::from_utf8_lossy(body)).unwrap_or_default(),
        "quoted-printable" => quoted_printable(body),
        _ => body.to_vec(),
    };
    Part {
        mime: mime_of(&content_type),
        charset: param(&content_type, "charset"),
        location: header(headers, "content-location").map(|l| l.trim().to_string()),
        id: header(headers, "content-id").map(|i| i.trim().trim_matches(['<', '>']).to_string()),
        body,
    }
}

/// Split a message (or part) into its unfolded header lines, `(lowercase name, value)`, and the
/// body after the first blank line.
fn split_headers(bytes: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    let start = bytes.iter().position(|c| !matches!(c, b'\r' | b'\n')).unwrap_or(bytes.len());
    let bytes = bytes.get(start..).unwrap_or_default();
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut at = 0;
    loop {
        let rest = bytes.get(at..).unwrap_or_default();
        let (line, next) = match rest.iter().position(|&c| c == b'\n') {
            Some(i) => (rest.get(..i).unwrap_or_default(), at + i + 1),
            None => (rest, bytes.len()),
        };
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || rest.is_empty() {
            return (headers, bytes.get(next.min(bytes.len())..).unwrap_or_default());
        }
        let text = String::from_utf8_lossy(line);
        if text.starts_with([' ', '\t']) {
            if let Some((_, v)) = headers.last_mut() {
                v.push(' ');
                v.push_str(text.trim());
            }
        } else if let Some((k, v)) = text.split_once(':') {
            if headers.len() >= MAX_HEADER_LINES {
                return (headers, bytes.get(next.min(bytes.len())..).unwrap_or_default());
            }
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        } else {
            // Not a header: no header section, the whole thing is the body.
            return (Vec::new(), bytes);
        }
        at = next;
    }
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

/// `type/subtype` of a Content-Type value, lowercase.
fn mime_of(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()
}

/// A parameter of a header value (`boundary="…"`), quotes removed.
fn param(value: &str, name: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().trim_matches('"').to_string())
    })
}

/// The bodies between the `--boundary` lines (the preamble and epilogue left out).
fn split_parts<'a>(body: &'a [u8], boundary: &[u8]) -> Vec<&'a [u8]> {
    let delim = [b"--".as_slice(), boundary].concat();
    let mut parts = Vec::new();
    let mut current: Option<usize> = None;
    let mut at = 0;
    while at < body.len() {
        let rest = body.get(at..).unwrap_or_default();
        let len = rest.iter().position(|&c| c == b'\n').map_or(rest.len(), |i| i + 1);
        let line = rest.get(..len).unwrap_or_default();
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.starts_with(&delim) && trimmed.get(delim.len()..).is_some_and(|t| t.iter().all(|c| matches!(c, b'-' | b' ' | b'\t'))) {
            if let Some(s) = current {
                // The line break before the delimiter belongs to it.
                let end = at.saturating_sub(if body.get(..at).is_some_and(|b| b.ends_with(b"\r\n")) { 2 } else { 1 }).max(s);
                parts.push(body.get(s..end).unwrap_or_default());
            }
            if trimmed.get(delim.len()..).is_some_and(|t| t.starts_with(b"--")) || parts.len() >= MAX_PARTS {
                return parts;
            }
            current = Some(at + len);
        }
        at += len;
    }
    if let Some(s) = current {
        parts.push(body.get(s..).unwrap_or_default());
    }
    parts
}

/// Quoted-printable (RFC 2045): `=XX` bytes and `=` soft line breaks.
fn quoted_printable(b: &[u8]) -> Vec<u8> {
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c != b'=' {
            out.push(c);
            i += 1;
            continue;
        }
        match (b.get(i + 1).copied(), b.get(i + 2).copied()) {
            (Some(b'\r'), Some(b'\n')) => i += 3,
            (Some(b'\n'), _) => i += 2,
            (Some(h), Some(l)) if hex(h).is_some() && hex(l).is_some() => {
                out.push((hex(h).unwrap_or(0) * 16 + hex(l).unwrap_or(0)) as u8);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// A text part in its charset (or by the page's own `<meta charset>` and detection).
fn decode_text(body: &[u8], charset: Option<&str>) -> String {
    match charset.and_then(|c| encoding_rs::Encoding::for_label(c.trim().as_bytes())) {
        Some(enc) => enc.decode(body).0.into_owned(),
        None => crate::html::decode(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::{InlineObject, StoryRef};

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF,
        0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60,
        0x82,
    ];

    /// A message the way a Single File Web Page is laid out: a quoted-printable page in
    /// windows-1252 and a base64 picture it names relative to its own location.
    fn message(src: &str) -> Vec<u8> {
        let b64 = crate::model::base64_encode(PNG);
        format!(
            "MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"----=_NextPart_01DA\"\r\n\r\nThis document is a Single File Web Page.\r\n\r\n\
             ------=_NextPart_01DA\r\nContent-Location: file:///C:/Export/report.htm\r\nContent-Transfer-Encoding: quoted-printable\r\nContent-Type: text/html; charset=\"windows-1252\"\r\n\r\n\
             <html><body><p class=3DMsoNormal><b>Caf=E9 report</b> for =\r\nSeptember</p><p><img src=3D\"{src}\"></p></body></html>\r\n\r\n\
             ------=_NextPart_01DA\r\nContent-Location: file:///C:/Export/report_files/image001.png\r\nContent-Transfer-Encoding: base64\r\nContent-Type: image/png\r\n\r\n{b64}\r\n\r\n\
             ------=_NextPart_01DA--\r\n"
        )
        .into_bytes()
    }

    fn pictures(doc: &Document) -> usize {
        doc.body.iter().filter_map(|b| b.as_para()).flat_map(|p| p.objects.iter()).filter(|o| matches!(o, InlineObject::Image { .. })).count()
    }

    #[test]
    fn a_single_file_web_page_opens_with_its_text_and_pictures() {
        for src in ["report_files/image001.png", "./report_files/image001.png", "file:///C:/Export/report_files/image001.png"] {
            let bytes = message(src);
            assert!(is_mhtml(&bytes));
            let doc = import_on(&bytes, &Default::default()).unwrap();
            let text = doc.plain_text(StoryRef::Body);
            assert!(text.contains("Café report for September"), "{text:?}");
            assert_eq!(pictures(&doc), 1, "{src}");
        }
        // By Content-ID.
        let cid = String::from_utf8(message("cid:pic1")).unwrap().replace("Content-Type: image/png", "Content-Type: image/png\r\nContent-ID: <pic1>");
        assert_eq!(pictures(&import_on(cid.as_bytes(), &Default::default()).unwrap()), 1);
    }

    #[test]
    fn other_text_is_not_mhtml_and_broken_messages_never_panic() {
        assert!(!is_mhtml(b"<html><body>MIME-Version: 1.0</body></html>"));
        assert!(!is_mhtml(b"Dear team,\r\nMIME-Version: 1.0 is the subject\r\n"));
        assert!(!is_mhtml(b"MIME-Version: 1.0\r\nContent-Type: image/png\r\n\r\n"));
        assert!(
            import_on(
                b"MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=x\r\n\r\n--x\r\nContent-Type: image/png\r\n\r\nzz\r\n--x--",
                &Default::default()
            )
            .is_err()
        );
        let whole = message("report_files/image001.png");
        for n in (0..whole.len()).step_by(7) {
            let _ = import_on(whole.get(..n).unwrap_or_default(), &Default::default());
        }
        for junk in [&b"=\r"[..], b"=", b"=Z", b"=4", b"\xff\xfe=0", b"\r\n\r\n", b":::\r\n\r\n"] {
            let _ = quoted_printable(junk);
            let _ = import_on(junk, &Default::default());
        }
    }
}
