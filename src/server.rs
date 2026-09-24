use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyDataStream;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;

use crate::body::Body;
use crate::crawler;
use crate::store::{ID_LEN, Saved, Source, Store, UploadError};

const X_ROBOTS_TAG: HeaderName = HeaderName::from_static("x-robots-tag");
const NO_ROBOTS: HeaderValue = HeaderValue::from_static(
    "noindex, nofollow, noarchive, nosnippet, noimageindex, noai, noimageai",
);
const IMMUTABLE: HeaderValue = HeaderValue::from_static("public, max-age=31536000, immutable");

pub struct App {
    store: Store,
    token: String,
    public_url: Option<String>,
    robots_txt: Bytes,
}

pub async fn run(app: &'static App, addr: SocketAddr) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!("listening on http://{addr}");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(e) => {
                // Usually out of file descriptors; back off instead of spinning.
                eprintln!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let service =
                service_fn(move |req| async move { Ok::<_, Infallible>(app.handle(req).await) });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

impl App {
    pub fn new(store: Store, token: String, public_url: Option<String>) -> Self {
        Self {
            store,
            token,
            public_url,
            robots_txt: Bytes::from(crawler::robots_txt()),
        }
    }

    async fn handle(&self, req: Request<Incoming>) -> Response<Body> {
        let (parts, body) = req.into_parts();
        let mut response = match parts.method {
            Method::GET | Method::HEAD => self.view(&parts).await,
            Method::PUT | Method::POST => self.upload(&parts, body).await,
            _ => status(StatusCode::METHOD_NOT_ALLOWED),
        };
        response.headers_mut().insert(X_ROBOTS_TAG, NO_ROBOTS);
        response
    }

    async fn view(&self, req: &Parts) -> Response<Body> {
        let path = req.uri.path();
        if path == "/robots.txt" {
            return text(StatusCode::OK, self.robots_txt.clone());
        }
        let user_agent = req
            .headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok());
        if user_agent.is_some_and(crawler::is_crawler) {
            return status(StatusCode::FORBIDDEN);
        }
        let Some(id) = parse_id(path) else {
            return status(StatusCode::NOT_FOUND);
        };

        let etag = HeaderValue::from_str(&format!("\"{id}\"")).unwrap();
        // Files never change, so a known ETag is answered without touching the disk.
        let if_none_match = req
            .headers
            .get(header::IF_NONE_MATCH)
            .map(HeaderValue::as_bytes);
        if if_none_match
            .is_some_and(|tags| tags.windows(etag.len()).any(|tag| tag == etag.as_bytes()))
        {
            return cached(status(StatusCode::NOT_MODIFIED), etag);
        }

        let stored = match self.store.get(id).await {
            Ok(Some(stored)) => stored,
            Ok(None) => return status(StatusCode::NOT_FOUND),
            Err(e) => {
                eprintln!("reading {id}: {e}");
                return status(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        let total = stored.body_len;
        let range = req
            .headers
            .get(header::RANGE)
            .filter(|_| req.headers.get(header::IF_RANGE).is_none_or(|v| *v == etag))
            .and_then(|v| v.to_str().ok())
            .map_or(Range::Full, |v| parse_range(v, total));
        let (code, start, len) = match range {
            Range::Full => (StatusCode::OK, 0, total),
            Range::Partial { start, len } => (StatusCode::PARTIAL_CONTENT, start, len),
            Range::Unsatisfiable => {
                let mut response = status(StatusCode::RANGE_NOT_SATISFIABLE);
                let content_range = HeaderValue::from_str(&format!("bytes */{total}")).unwrap();
                response
                    .headers_mut()
                    .insert(header::CONTENT_RANGE, content_range);
                return response;
            }
        };

        let body = if req.method == Method::HEAD {
            Body::Empty
        } else {
            Body::from_file(stored.file, stored.head, stored.body_offset + start, len)
        };
        let mut response = Response::new(body);
        *response.status_mut() = code;
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, stored.content_type);
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        if code == StatusCode::PARTIAL_CONTENT {
            let end = start + len - 1;
            let content_range =
                HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap();
            headers.insert(header::CONTENT_RANGE, content_range);
        }
        cached(response, etag)
    }

    async fn upload(&self, req: &Parts, body: Incoming) -> Response<Body> {
        if !self.authorized(&req.headers) {
            let mut response = status(StatusCode::UNAUTHORIZED);
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return response;
        }
        let declared_type = req
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let saved = match declared_type.as_deref().map(multer::parse_boundary) {
            Some(Ok(boundary)) => {
                let multipart = multer::Multipart::new(BodyDataStream::new(body), boundary);
                self.save_form_file(multipart).await
            }
            _ => {
                let file_name = raw_file_name(req.uri.path());
                self.store
                    .save(Source::Raw(body), file_name, declared_type)
                    .await
            }
        };
        match saved {
            Ok(saved) => {
                let url = format!("{}/{}", self.base_url(&req.headers), saved.name);
                eprintln!(
                    "stored {url} ({} bytes, {})",
                    saved.size, saved.content_type
                );
                let location = HeaderValue::from_str(&url);
                let mut response = text(StatusCode::CREATED, format!("{url}\n"));
                if let Ok(location) = location {
                    response.headers_mut().insert(header::LOCATION, location);
                }
                response
            }
            Err(UploadError::BadRequest(reason)) => {
                text(StatusCode::BAD_REQUEST, format!("{reason}\n"))
            }
            Err(UploadError::Io(e)) => {
                eprintln!("storing upload: {e}");
                status(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }

    async fn save_form_file(
        &self,
        mut multipart: multer::Multipart<'static>,
    ) -> Result<Saved, UploadError> {
        let bad_request = |e: multer::Error| UploadError::BadRequest(e.to_string());
        while let Some(field) = multipart.next_field().await.map_err(bad_request)? {
            if field.file_name().is_some() || field.name() == Some("file") {
                let file_name = field.file_name().map(str::to_owned);
                let declared_type = field.content_type().map(ToString::to_string);
                return self
                    .store
                    .save(Source::Field(field), file_name, declared_type)
                    .await;
            }
        }
        Err(UploadError::BadRequest("the form has no file field".into()))
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .map(HeaderValue::as_bytes)
        else {
            return false;
        };
        value.len() > 7
            && value[..7].eq_ignore_ascii_case(b"bearer ")
            && constant_time_eq(value[7..].trim_ascii(), self.token.as_bytes())
    }

    fn base_url(&self, headers: &HeaderMap) -> String {
        if let Some(url) = &self.public_url {
            return url.clone();
        }
        let first_value = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let https =
            first_value("x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https"));
        let host = first_value("x-forwarded-host")
            .or_else(|| first_value("host"))
            .unwrap_or("localhost");
        format!("{}://{host}", if https { "https" } else { "http" })
    }
}

fn status(code: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::Empty);
    *response.status_mut() = code;
    response
}

fn text(code: StatusCode, body: impl Into<Bytes>) -> Response<Body> {
    let mut response = Response::new(Body::full(body));
    *response.status_mut() = code;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn cached(mut response: Response<Body>, etag: HeaderValue) -> Response<Body> {
    response.headers_mut().insert(header::ETAG, etag);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, IMMUTABLE);
    response
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

// Only the id is looked up; the extension is there to make the url recognisable.
fn parse_id(path: &str) -> Option<&str> {
    let name = path.strip_prefix('/').filter(|name| !name.contains('/'))?;
    let id = name.split_once('.').map_or(name, |(id, _)| id);
    let valid = id.len() == ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    valid.then_some(id)
}

fn raw_file_name(path: &str) -> Option<String> {
    let name = percent_decode(path.rsplit('/').next()?);
    (!name.is_empty()).then_some(name)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = bytes
            .get(i + 1..i + 3)
            .filter(|_| bytes[i] == b'%')
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, PartialEq)]
enum Range {
    Full,
    Partial { start: u64, len: u64 },
    Unsatisfiable,
}

// Multiple ranges get the whole file, which RFC 9110 allows.
fn parse_range(header: &str, total: u64) -> Range {
    let Some((first, last)) = header
        .strip_prefix("bytes=")
        .filter(|spec| !spec.contains(','))
        .and_then(|spec| spec.split_once('-'))
    else {
        return Range::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    let last_byte = total.saturating_sub(1);
    let (start, end) = match (first.parse::<u64>(), last.parse::<u64>()) {
        (Ok(start), Ok(end)) if start <= end => (start, end.min(last_byte)),
        (Ok(start), Err(_)) if last.is_empty() => (start, last_byte),
        (Err(_), Ok(0)) if first.is_empty() => return Range::Unsatisfiable,
        (Err(_), Ok(suffix)) if first.is_empty() => (total.saturating_sub(suffix), last_byte),
        _ => return Range::Full,
    };
    if start >= total {
        return Range::Unsatisfiable;
    }
    Range::Partial {
        start,
        len: end - start + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_parsed_from_paths() {
        assert_eq!(parse_id("/abcdef012345.png"), Some("abcdef012345"));
        assert_eq!(parse_id("/abcdef012345.Dockerfile"), Some("abcdef012345"));
        assert_eq!(parse_id("/abcdef012345"), Some("abcdef012345"));
        assert_eq!(parse_id("/ABCDEF012345.png"), None);
        assert_eq!(parse_id("/abcdef01234.png"), None);
        assert_eq!(parse_id("/abcdef012345.png/x"), None);
        assert_eq!(parse_id("/"), None);
    }

    #[test]
    fn ranges() {
        assert_eq!(
            parse_range("bytes=0-99", 1000),
            Range::Partial { start: 0, len: 100 }
        );
        assert_eq!(
            parse_range("bytes=900-", 1000),
            Range::Partial {
                start: 900,
                len: 100
            }
        );
        assert_eq!(
            parse_range("bytes=-100", 1000),
            Range::Partial {
                start: 900,
                len: 100
            }
        );
        assert_eq!(
            parse_range("bytes=-5000", 1000),
            Range::Partial {
                start: 0,
                len: 1000
            }
        );
        assert_eq!(
            parse_range("bytes=500-5000", 1000),
            Range::Partial {
                start: 500,
                len: 500
            }
        );
        assert_eq!(parse_range("bytes=1000-", 1000), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 1000), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), Range::Full);
        assert_eq!(parse_range("bytes=9-3", 1000), Range::Full);
        assert_eq!(parse_range("items=0-1", 1000), Range::Full);
    }

    #[test]
    fn raw_upload_names_come_from_the_last_path_segment() {
        assert_eq!(
            raw_file_name("/my%20photo.png").as_deref(),
            Some("my photo.png")
        );
        assert_eq!(
            raw_file_name("/dir/Dockerfile").as_deref(),
            Some("Dockerfile")
        );
        assert_eq!(raw_file_name("/100%").as_deref(), Some("100%"));
        assert_eq!(raw_file_name("/"), None);
    }

    #[test]
    fn tokens_compare_exactly() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
    }
}
