//! Client-side encryption for Shelf (BLUEPRINT §9). age x25519, 64 KiB chunks.
use age::{secrecy::ExposeSecret, x25519::Identity, Decryptor, Encryptor};
use anyhow::Context;
use object_store::{Attributes, ObjectStore};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use crate::{Path, store};

pub use age::x25519;

const CHUNK: u64 = 64 * 1024;
const TAG: u64 = 16;
const ECHUNK: u64 = CHUNK + TAG;
/// Bytes to fetch from the start of an object to cover its header (age adds a random-length
/// "grease" stanza, so the header length varies per file: ~270-450 B; pass the result to `header_len`).
pub const HEADER_PROBE: u64 = 1024;

// ---- identity -------------------------------------------------------------------------------

pub fn new_identity() -> Identity {
    Identity::generate()
}

fn entry(bucket: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new("shelf", &format!("age-identity:{bucket}"))
}

pub fn save_identity(bucket: &str, id: &Identity) -> anyhow::Result<()> {
    entry(bucket)?.set_password(id.to_string().expose_secret())?;
    Ok(())
}

/// `Ok(None)` if no identity is stored for this bucket.
pub fn load_identity(bucket: &str) -> anyhow::Result<Option<Identity>> {
    match entry(bucket)?.get_password() {
        Ok(s) => Ok(Some(s.parse::<Identity>().map_err(|e| anyhow::anyhow!("bad stored identity: {e}"))?)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Text shown once for the user to save. Contains the secret key.
pub fn recovery_text(id: &Identity) -> String {
    format!(
        "Shelf recovery key\n\
         Anyone with this key can read your encrypted files; if you lose it they cannot be recovered.\n\n\
         # public key: {}\n{}\n",
        id.to_public(),
        id.to_string().expose_secret()
    )
}

// ---- key naming -----------------------------------------------------------------------------

pub const SUFFIX: &str = ".age";

pub fn is_encrypted(key: &str) -> bool {
    key.ends_with(SUFFIX)
}
pub fn add_age(key: &str) -> String {
    format!("{key}{SUFFIX}")
}
pub fn strip_age(key: &str) -> &str {
    key.strip_suffix(SUFFIX).unwrap_or(key)
}

// ---- streaming encrypt ----------------------------------------------------------------------

/// Wraps any writer: plaintext in, ciphertext out. Call `.finish()` when done.
pub fn encryptor<W: Write>(id: &Identity, out: W) -> io::Result<age::stream::StreamWriter<W>> {
    let r = id.to_public();
    Encryptor::with_recipients(std::iter::once(&r as &dyn age::Recipient))
        .map_err(io::Error::other)?
        .wrap_output(out)
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);
impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Pull-style encryption: reads plaintext 1 MiB at a time and yields ciphertext pieces to feed
/// into `WriteMultipart::write` (memory stays ~1 MiB). Blocking reads; run in spawn_blocking.
pub struct Encrypting<R> {
    src: R,
    w: Option<age::stream::StreamWriter<Shared>>,
    out: Shared,
}
impl<R: Read> Encrypting<R> {
    pub fn new(id: &Identity, src: R) -> io::Result<Self> {
        let out = Shared::default();
        Ok(Self { src, w: Some(encryptor(id, out.clone())?), out })
    }
    /// Next ciphertext piece, `None` when done.
    pub fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 1 << 20];
        while let Some(w) = self.w.as_mut() {
            let n = self.src.read(&mut buf)?;
            if n == 0 {
                self.w.take().unwrap().finish()?;
            } else {
                w.write_all(&buf[..n])?;
            }
            let piece = std::mem::take(&mut *self.out.0.lock().unwrap());
            if !piece.is_empty() {
                return Ok(Some(piece));
            }
        }
        Ok(None)
    }
}

/// Streaming decrypt: ciphertext reader -> any writer (e.g. a File). Returns plaintext bytes written.
pub fn decrypt_to<R: Read, W: Write>(id: &Identity, input: R, mut out: W) -> io::Result<u64> {
    let mut r = Decryptor::new(input)
        .map_err(io::Error::other)?
        .decrypt(std::iter::once(id as &dyn age::Identity))
        .map_err(io::Error::other)?;
    io::copy(&mut r, &mut out)
}

// ---- object store glue ----------------------------------------------------------------------

/// Encrypts `local` on a blocking thread and feeds the ciphertext, through a channel, into a
/// multipart upload (aborted on error). Progress counts ciphertext bytes, capped at the file size.
pub async fn upload_encrypted(
    store: &dyn ObjectStore,
    local: &std::path::Path,
    key: &Path,
    id: Identity,
    on_progress: impl Fn(u64),
) -> anyhow::Result<()> {
    let total = tokio::fs::metadata(local).await?.len();
    let mut write = store::start_multipart(store, key, Attributes::new()).await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<io::Result<Vec<u8>>>(4);
    let path = local.to_owned();
    let reader = tokio::task::spawn_blocking(move || {
        let run = || -> io::Result<()> {
            let mut enc = Encrypting::new(&id, std::fs::File::open(&path)?)?;
            while let Some(chunk) = enc.next_chunk()? {
                if tx.blocking_send(Ok(chunk)).is_err() {
                    break; // the uploader gave up
                }
            }
            Ok(())
        };
        if let Err(error) = run() {
            let _ = tx.blocking_send(Err(error));
        }
    });
    let copied: anyhow::Result<()> = async {
        let mut sent = 0;
        while let Some(chunk) = rx.recv().await {
            let chunk = chunk?;
            write.wait_for_capacity(4).await?;
            sent += chunk.len() as u64;
            write.write(&chunk);
            on_progress(sent.min(total));
        }
        Ok(())
    }
    .await;
    drop(rx); // unblocks the reader if we bailed out
    let _ = reader.await;
    store::finish_multipart(write, copied).await?;
    Ok(())
}

/// Downloads the ciphertext next to `dest`, decrypts it into `dest` on a blocking thread, and
/// removes the temp file.
pub async fn download_decrypted(
    store: &dyn ObjectStore,
    key: &Path,
    dest: &std::path::Path,
    id: Identity,
) -> anyhow::Result<()> {
    let name = dest.file_name().context("destination has no name")?.to_string_lossy();
    let tmp = dest.with_file_name(format!(".{name}.shelf-tmp"));
    let result = async {
        store::download(store, key, &tmp).await?;
        let (from, to) = (tmp.clone(), dest.to_owned());
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let mut out = io::BufWriter::new(std::fs::File::create(to)?);
            decrypt_to(&id, std::fs::File::open(from)?, &mut out)?;
            out.flush()
        })
        .await??;
        anyhow::Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    if result.is_err() {
        let _ = std::fs::remove_file(dest); // never leave a half-decrypted file
    }
    result
}

// ---- range mapping --------------------------------------------------------------------------

/// Length of the age header + 16-byte payload nonce, parsed from the first `HEADER_PROBE` bytes
/// of the object. Assumes the header is the text form ending in a `--- <mac>` line (never armored).
/// Cache it per object.
pub fn header_len(prefix: &[u8]) -> Option<u64> {
    let mac = prefix.windows(5).position(|w| w == b"\n--- ")? + 1;
    let nl = prefix[mac..].iter().position(|&b| b == b'\n')?;
    let end = (mac + nl + 1 + 16) as u64;
    (end <= prefix.len() as u64).then_some(end)
}

/// Plaintext size from the ciphertext object size and `header_len`.
pub fn plaintext_size(obj_size: u64, hlen: u64) -> u64 {
    let body = obj_size.saturating_sub(hlen);
    body - body.div_ceil(ECHUNK) * TAG
}

/// Ciphertext byte range covering plaintext `[off, off+len)`: whole 64 KiB(+16 B tag) chunks
/// after the header, clamped to the object.
pub fn cipher_range(off: u64, len: u64, obj_size: u64, hlen: u64) -> Range<u64> {
    let first = off / CHUNK;
    let last = (off + len.max(1) - 1) / CHUNK;
    (hlen + first * ECHUNK).min(obj_size)..(hlen + (last + 1) * ECHUNK).min(obj_size)
}

/// Presents header + one fetched window as the full ciphertext file, so age's seekable
/// StreamReader can do the chunk/nonce work. Reads outside header/window return EOF.
struct Sparse<'a> {
    hdr: &'a [u8],
    win: &'a [u8],
    win_start: u64,
    total: u64,
    pos: u64,
}
impl Read for Sparse<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (src, base) = if self.pos < self.hdr.len() as u64 { (self.hdr, 0) } else { (self.win, self.win_start) };
        let Some(rest) = self.pos.checked_sub(base).and_then(|o| src.get(o as usize..)) else { return Ok(0) };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for Sparse<'_> {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.pos = match p {
            SeekFrom::Start(n) => n,
            SeekFrom::End(d) => self.total.saturating_add_signed(d),
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

/// Decrypt plaintext `[off, off+len)` given `header` (first `header_len` bytes), `window` (bytes of
/// `cipher_range(off, len, obj_size, header.len())`) and the object's total size. Clamped at EOF.
pub fn decrypt_range(id: &Identity, header: &[u8], window: &[u8], off: u64, len: u64, obj_size: u64) -> io::Result<Vec<u8>> {
    let hlen = header.len() as u64;
    let size = plaintext_size(obj_size, hlen);
    let len = len.min(size.saturating_sub(off));
    if len == 0 {
        return Ok(vec![]);
    }
    let win_start = cipher_range(off, len, obj_size, hlen).start;
    let mut r = Decryptor::new(Sparse { hdr: header, win: window, win_start, total: obj_size, pos: 0 })
        .map_err(io::Error::other)?
        .decrypt(std::iter::once(id as &dyn age::Identity))
        .map_err(io::Error::other)?;
    // A chunk-aligned seek makes age fetch the *last* chunk to learn the length (not in our
    // window), so land 1 byte into the chunk, then step back within it.
    if off % CHUNK == 0 && off != 0 {
        r.seek(SeekFrom::Start(off + 1))?;
        r.seek(SeekFrom::Current(-1))?;
    } else {
        r.seek(SeekFrom::Start(off))?;
    }
    let mut out = vec![0; len as usize];
    r.read_exact(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + i / 7) as u8).collect()
    }
    fn enc(id: &Identity, pt: &[u8]) -> Vec<u8> {
        let mut e = Encrypting::new(id, pt).unwrap();
        let mut ct = vec![];
        while let Some(c) = e.next_chunk().unwrap() {
            ct.extend(c);
        }
        ct
    }

    /// Probe prefix (as a client would fetch it) and parsed header length.
    fn hdr(ct: &[u8]) -> (&[u8], u64) {
        let p = &ct[..ct.len().min(HEADER_PROBE as usize)];
        (p, header_len(p).unwrap())
    }

    #[test]
    fn round_trip() {
        let id = new_identity();
        for n in [0, 1, 64 << 10, (64 << 10) + 1, (1 << 20) + 7] {
            let pt = data(n);
            let ct = enc(&id, &pt);
            assert_eq!(plaintext_size(ct.len() as u64, hdr(&ct).1), n as u64);
            let mut out = vec![];
            assert_eq!(decrypt_to(&id, &ct[..], &mut out).unwrap(), n as u64);
            assert_eq!(out, pt, "n={n}");
        }
        assert_eq!(strip_age(&add_age("a/b.txt")), "a/b.txt");
        assert!(is_encrypted("a.age") && !is_encrypted("a"));
    }

    #[tokio::test]
    async fn store_round_trip() {
        use object_store::{ObjectStore, memory::InMemory};
        let (id, dir) = (new_identity(), tempfile::tempdir().unwrap());
        let (src, back) = (dir.path().join("a.bin"), dir.path().join("back.bin"));
        let pt = data((9 << 20) + 5); // more than one 8 MiB part
        std::fs::write(&src, &pt).unwrap();
        let store = InMemory::new();
        let key = Path::from("x/a.bin.age");
        upload_encrypted(&store, &src, &key, id.clone(), |_| {}).await.unwrap();
        let stored = store.get(&key).await.unwrap().bytes().await.unwrap();
        assert!(stored.len() > pt.len() && !stored.windows(64).any(|w| pt[..64] == *w));
        download_decrypted(&store, &key, &back, id).await.unwrap();
        assert_eq!(std::fs::read(&back).unwrap(), pt);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2); // no temp file left
        // wrong key: error, and nothing left behind
        let other = dir.path().join("other.bin");
        assert!(download_decrypted(&store, &key, &other, new_identity()).await.is_err());
        assert!(!other.exists());
    }

    #[test]
    fn range() {
        let id = new_identity();
        let pt = data((3 << 16) + 100);
        let ct = enc(&id, &pt);
        let sz = ct.len() as u64;
        let (p, hl) = hdr(&ct);
        let hdr = &p[..hl as usize];
        // (off, len): across a boundary, aligned start, mid-chunk, last partial chunk, past EOF
        for (off, len) in [(65530u64, 20u64), (65536, 10), (70000, 5), (3 << 16, 1000), (100, 3 << 16), (pt.len() as u64 - 3, 50)] {
            let r = cipher_range(off, len, sz, hl);
            let got = decrypt_range(&id, hdr, &ct[r.start as usize..r.end as usize], off, len, sz).unwrap();
            let end = (off + len).min(pt.len() as u64) as usize;
            assert_eq!(got, &pt[off as usize..end], "off={off} len={len}");
        }
    }
}
