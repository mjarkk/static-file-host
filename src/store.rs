//! Each upload is one file at `<root>/<first two id chars>/<id>`, holding the
//! content type, a newline, then the uploaded bytes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::mime;

pub const ID_LEN: usize = 12;
const ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const TOKEN_LEN: usize = 32;
const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const HEAD_READ: u64 = 64 * 1024;
const WRITE_BUFFER: usize = 256 * 1024;

pub struct Store {
    root: PathBuf,
    tmp: PathBuf,
}

pub struct Stored {
    pub file: File,
    /// The first bytes of the file, header included.
    pub head: Bytes,
    pub body_offset: u64,
    pub body_len: u64,
    pub content_type: HeaderValue,
}

pub struct Saved {
    /// `<id>.<extension>`, or just `<id>` when no extension could be found.
    pub name: String,
    pub content_type: String,
    pub size: u64,
}

#[allow(clippy::large_enum_variant)]
pub enum Source<'a> {
    Raw(Incoming),
    Field(multer::Field<'a>),
}

pub enum UploadError {
    BadRequest(String),
    Io(io::Error),
}

impl From<io::Error> for UploadError {
    fn from(e: io::Error) -> Self {
        UploadError::Io(e)
    }
}

impl Store {
    pub fn open(root: PathBuf) -> io::Result<Self> {
        let tmp = root.join("tmp");
        fs::create_dir_all(&tmp)?;
        for entry in fs::read_dir(&tmp)? {
            let _ = fs::remove_file(entry?.path());
        }
        Ok(Self { root, tmp })
    }

    pub async fn get(&self, id: &str) -> io::Result<Option<Stored>> {
        let path = self.root.join(&id[..2]).join(id);
        tokio::task::spawn_blocking(move || read_head(&path)).await?
    }

    pub async fn save(
        &self,
        mut source: Source<'_>,
        file_name: Option<String>,
        declared_type: Option<String>,
    ) -> Result<Saved, UploadError> {
        let mut head = Vec::new();
        while head.len() < mime::SNIFF_LEN {
            match source.chunk().await? {
                Some(chunk) => head.extend_from_slice(&chunk),
                None => break,
            }
        }
        let extension = file_name.as_deref().and_then(mime::file_extension);
        let content_type =
            mime::content_type(declared_type.as_deref(), extension.as_deref(), &head);
        let extension =
            extension.or_else(|| infer::get(&head).map(|kind| kind.extension().to_owned()));

        let temp_path = self.tmp.join(random_string(ID_ALPHABET, 16));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await?;
        let temp = RemoveOnDrop(temp_path);
        let mut out = BufWriter::with_capacity(WRITE_BUFFER, file);
        out.write_all(content_type.as_bytes()).await?;
        out.write_all(b"\n").await?;
        out.write_all(&head).await?;
        let mut size = head.len() as u64;
        drop(head);
        while let Some(chunk) = source.chunk().await? {
            size += chunk.len() as u64;
            out.write_all(&chunk).await?;
        }
        out.flush().await?;
        out.into_inner().sync_data().await?;

        let root = self.root.clone();
        let id = tokio::task::spawn_blocking(move || publish(&root, temp))
            .await
            .map_err(io::Error::from)??;
        let name = match extension {
            Some(extension) => format!("{id}.{extension}"),
            None => id,
        };
        Ok(Saved {
            name,
            content_type,
            size,
        })
    }
}

impl Source<'_> {
    async fn chunk(&mut self) -> Result<Option<Bytes>, UploadError> {
        match self {
            Source::Raw(body) => loop {
                match body.frame().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(UploadError::BadRequest(e.to_string())),
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            return Ok(Some(data));
                        }
                    }
                }
            },
            Source::Field(field) => field
                .chunk()
                .await
                .map_err(|e| UploadError::BadRequest(e.to_string())),
        }
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn read_head(path: &Path) -> io::Result<Option<Stored>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let mut head = vec![0; len.min(HEAD_READ) as usize];
    file.read_exact(&mut head)?;
    let head = Bytes::from(head);
    let newline = head
        .iter()
        .position(|&b| b == b'\n')
        .ok_or(io::ErrorKind::InvalidData)?;
    let content_type = HeaderValue::from_maybe_shared(head.slice(..newline))
        .map_err(|_| io::ErrorKind::InvalidData)?;
    let body_offset = newline as u64 + 1;
    Ok(Some(Stored {
        file,
        head,
        body_offset,
        body_len: len - body_offset,
        content_type,
    }))
}

fn publish(root: &Path, temp: RemoveOnDrop) -> io::Result<String> {
    loop {
        let id = random_string(ID_ALPHABET, ID_LEN);
        let dir = root.join(&id[..2]);
        let path = dir.join(&id);
        let linked = match fs::hard_link(&temp.0, &path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&dir)?;
                File::open(root)?.sync_all()?;
                fs::hard_link(&temp.0, &path)
            }
            linked => linked,
        };
        match linked {
            Ok(()) => {
                File::open(&dir)?.sync_all()?;
                return Ok(id);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Returns the token and whether it was just created. Fails if the token file exists but is empty.
pub fn load_or_create_token(root: &Path) -> io::Result<(String, bool)> {
    let path = root.join("token");
    match fs::read_to_string(&path) {
        Ok(token) if token.trim().is_empty() => {
            Err(io::Error::other(format!("{} is empty", path.display())))
        }
        Ok(token) => Ok((token.trim().to_owned(), false)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            let token = random_string(TOKEN_ALPHABET, TOKEN_LEN);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(format!("{token}\n").as_bytes())?;
            file.sync_all()?;
            Ok((token, true))
        }
        Err(e) => Err(e),
    }
}

fn random_string(alphabet: &[u8], len: usize) -> String {
    // Bytes at or above `limit` are dropped so every character is equally likely.
    let limit = 256 - 256 % alphabet.len();
    let mut out = String::with_capacity(len);
    let mut bytes = [0u8; 64];
    while out.len() < len {
        getrandom::fill(&mut bytes).expect("OS random number generator failed");
        let missing = len - out.len();
        out.extend(
            bytes
                .iter()
                .filter(|&&b| usize::from(b) < limit)
                .map(|&b| char::from(alphabet[usize::from(b) % alphabet.len()]))
                .take(missing),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_use_only_the_id_alphabet() {
        let id = random_string(ID_ALPHABET, ID_LEN);
        assert_eq!(id.len(), ID_LEN);
        assert!(id.bytes().all(|b| ID_ALPHABET.contains(&b)));
    }
}
