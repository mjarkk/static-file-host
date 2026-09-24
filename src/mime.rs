use hyper::header::HeaderValue;

pub const SNIFF_LEN: usize = 8192;
// Stored as the file's first line, which must fit in store::HEAD_READ.
const MAX_CONTENT_TYPE_LEN: usize = 255;
const MAX_EXTENSION_LEN: usize = 32;
// What clients send when they don't know the type; curl --data-binary sends the form one.
const PLACEHOLDER_TYPES: [&str; 3] = [
    "application/octet-stream",
    "binary/octet-stream",
    "application/x-www-form-urlencoded",
];

pub fn content_type(declared: Option<&str>, extension: Option<&str>, head: &[u8]) -> String {
    let head = &head[..head.len().min(SNIFF_LEN)];
    let content_type = match declared.map(str::trim).filter(|ct| is_usable(ct)) {
        Some(ct) => ct.to_owned(),
        None => detect(extension, head),
    };
    with_utf8_charset(content_type, head)
}

/// What follows the last dot, or the whole name when there is none (`Dockerfile`),
/// reduced to url-safe characters.
pub fn file_extension(file_name: &str) -> Option<String> {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let extension: String = base
        .rsplit('.')
        .next()
        .unwrap_or(base)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(MAX_EXTENSION_LEN)
        .collect();
    (!extension.is_empty()).then_some(extension)
}

fn is_usable(content_type: &str) -> bool {
    let essence = essence(content_type);
    content_type.len() <= MAX_CONTENT_TYPE_LEN
        && essence.contains('/')
        && HeaderValue::from_str(content_type).is_ok()
        && !PLACEHOLDER_TYPES
            .iter()
            .any(|placeholder| essence.eq_ignore_ascii_case(placeholder))
}

fn essence(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or("").trim()
}

fn detect(extension: Option<&str>, head: &[u8]) -> String {
    // infer's text matches rank below the extension: it calls any `<?xml` file text/xml, SVGs included.
    let magic = infer::get(head);
    if let Some(kind) = magic.filter(|kind| kind.matcher_type() != infer::MatcherType::Text) {
        return kind.mime_type().to_owned();
    }
    let text = is_text(head);
    extension
        .and_then(|extension| mime_guess::from_ext(extension).first_raw())
        .filter(|guess| *guess != "application/octet-stream")
        // TypeScript's .ts is text, not video/mp2t.
        .filter(|guess| !text || !is_media(guess))
        .or(magic.map(|kind| kind.mime_type()))
        .unwrap_or(if text {
            "text/plain"
        } else {
            "application/octet-stream"
        })
        .to_owned()
}

fn is_media(mime: &str) -> bool {
    ["image/", "audio/", "video/", "font/"]
        .iter()
        .any(|prefix| mime.starts_with(prefix))
        && !mime.ends_with("+xml")
}

fn is_text(head: &[u8]) -> bool {
    !head.contains(&0) && is_utf8(head)
}

fn is_utf8(head: &[u8]) -> bool {
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // No error_len means the sniff window cut the last character in half.
        Err(e) => e.error_len().is_none(),
    }
}

// Browsers decode text without a charset as windows-1252.
fn with_utf8_charset(content_type: String, head: &[u8]) -> String {
    let is_text_type = content_type
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/"));
    if is_text_type && !content_type.to_ascii_lowercase().contains("charset") && is_utf8(head) {
        format!("{content_type}; charset=utf-8")
    } else {
        content_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn declared_type_is_kept() {
        assert_eq!(
            content_type(Some("image/webp"), Some("png"), PNG),
            "image/webp"
        );
    }

    #[test]
    fn placeholder_types_are_detected_from_content() {
        assert_eq!(
            content_type(Some("application/octet-stream"), None, PNG),
            "image/png"
        );
        assert_eq!(
            content_type(Some("application/x-www-form-urlencoded"), None, PNG),
            "image/png"
        );
    }

    #[test]
    fn magic_bytes_beat_the_extension() {
        assert_eq!(content_type(None, Some("txt"), PNG), "image/png");
    }

    #[test]
    fn text_without_known_extension_is_plain_utf8() {
        let dockerfile = b"FROM alpine\nRUN echo \xc3\xa9\n";
        assert_eq!(
            content_type(None, Some("Dockerfile"), dockerfile),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn text_under_a_media_extension_is_plain_text() {
        assert_eq!(
            content_type(None, Some("ts"), b"export const x = 1;\n"),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn svg_with_xml_prolog_stays_an_image() {
        let svg = b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(content_type(None, Some("svg"), svg), "image/svg+xml");
    }

    #[test]
    fn declared_text_gets_a_charset_only_when_missing() {
        assert_eq!(
            content_type(Some("text/html"), None, b"<p>hi</p>"),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Some("text/html; charset=latin1"), None, b"hi"),
            "text/html; charset=latin1"
        );
    }

    #[test]
    fn unknown_binary_is_octet_stream() {
        assert_eq!(
            content_type(None, None, b"\x00\x01\x02\xff"),
            "application/octet-stream"
        );
    }

    #[test]
    fn extensions() {
        assert_eq!(file_extension("photo.PNG").as_deref(), Some("PNG"));
        assert_eq!(file_extension("Dockerfile").as_deref(), Some("Dockerfile"));
        assert_eq!(file_extension("a.b.c").as_deref(), Some("c"));
        assert_eq!(file_extension(".bashrc").as_deref(), Some("bashrc"));
        assert_eq!(
            file_extension("C:\\Users\\me\\cat.jpg").as_deref(),
            Some("jpg")
        );
        assert_eq!(file_extension("my file.tar gz").as_deref(), Some("targz"));
        assert_eq!(file_extension("trailing."), None);
    }
}
