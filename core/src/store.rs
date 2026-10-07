use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt};
use object_store::{
    Attributes, ClientOptions, ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, RetryConfig,
    WriteMultipart, aws::AmazonS3, aws::AmazonS3Builder, signer::Signer,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unicode_normalization::UnicodeNormalization;

use crate::{Path, crypt};

const PART: usize = 8 << 20;

/// Saved to ~/Library/Application Support/Shelf/config.json. Secrets (S3 secret, age identity)
/// are not here.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BucketConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    /// Uploads are age-encrypted and get a `.age` key suffix.
    #[serde(default)]
    pub encrypt: bool,
    #[serde(default = "yes")]
    pub check_updates: bool,
    /// Start Shelf at login (a LaunchAgent, see `login`). Applied on first connect.
    #[serde(default = "yes")]
    pub open_at_login: bool,
    /// Remount the Finder drive at login. Set by mounting, cleared by Eject.
    #[serde(default)]
    pub mount_at_login: bool,
    #[serde(default)]
    pub syncs: Vec<SyncPair>,
}

fn yes() -> bool {
    true
}

impl BucketConfig {
    pub fn new(endpoint: String, bucket: String, access_key_id: String) -> Self {
        Self { endpoint, bucket, access_key_id, encrypt: false, check_updates: true, open_at_login: true, mount_at_login: false, syncs: Vec::new() }
    }
}

/// A local folder kept in two-way sync with a bucket prefix ("" or "a/b/").
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SyncPair {
    pub folder: String,
    pub prefix: String,
}

impl SyncPair {
    /// Where this pair's sync base lives: `sync-<id>.json` next to the config.
    pub fn state_path(&self) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let id = Sha256::digest(format!("{}\0{}", self.folder, self.prefix));
        let id: String = id.iter().take(6).map(|b| format!("{b:02x}")).collect();
        Ok(support_dir()?.join(format!("sync-{id}.json")))
    }
}

/// S3 client for the bucket; concrete so it can also sign URLs.
pub type Store = Arc<AmazonS3>;

fn builder(cfg: &BucketConfig, secret: &str) -> AmazonS3Builder {
    AmazonS3Builder::new()
        .with_endpoint(&cfg.endpoint)
        .with_bucket_name(&cfg.bucket)
        .with_region("auto")
        .with_access_key_id(&cfg.access_key_id)
        .with_secret_access_key(secret)
        .with_virtual_hosted_style_request(false)
        .with_allow_http(cfg.endpoint.starts_with("http://"))
}

pub fn open(cfg: &BucketConfig, secret: &str) -> Result<Store> {
    Ok(Arc::new(builder(cfg, secret).build()?))
}

/// Like `open`, but gives up fast (10 s per request, one retry). For the Test/Connect check,
/// not for transfers: the timeout covers the whole request.
pub fn open_quick(cfg: &BucketConfig, secret: &str) -> Result<Store> {
    let retry = RetryConfig { max_retries: 1, retry_timeout: Duration::from_secs(15), ..Default::default() };
    let client = ClientOptions::new().with_timeout(Duration::from_secs(10)).with_connect_timeout(Duration::from_secs(5));
    Ok(Arc::new(builder(cfg, secret).with_retry(retry).with_client_options(client).build()?))
}

/// A short message for the UI. The full text stays available as `{error:#}`.
pub fn friendly(error: &anyhow::Error) -> String {
    let text = format!("{error:#}").to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if has(&["403", "accessdenied", "access denied", "invalidaccesskeyid", "signaturedoesnotmatch", "401"]) {
        "Access denied — check keys".into()
    } else if has(&["nosuchbucket", "404", "not found"]) {
        "Bucket not found — check the bucket name".into()
    } else if has(&["error sending request", "dns", "connect", "timed out", "timeout", "retry", "invalid url"]) {
        "Can't reach bucket — check endpoint".into()
    } else {
        error.to_string().chars().take(120).collect()
    }
}

// ---- config + keychain -------------------------------------------------------------------

pub fn support_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("no HOME")?;
    Ok(PathBuf::from(home).join("Library/Application Support/Shelf"))
}

fn config_path() -> Result<PathBuf> {
    Ok(support_dir()?.join("config.json"))
}

pub fn load_config() -> Option<BucketConfig> {
    serde_json::from_slice(&std::fs::read(config_path().ok()?).ok()?).ok()
}

pub fn save_config(cfg: &BucketConfig) -> Result<()> {
    let path = config_path()?;
    std::fs::create_dir_all(path.parent().context("config path has no parent")?)?;
    std::fs::write(path, serde_json::to_vec_pretty(cfg)?)?;
    Ok(())
}

pub fn clear_config() {
    if let Ok(path) = config_path() {
        let _ = std::fs::remove_file(path);
    }
}

fn entry(bucket: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new("shelf", bucket)?)
}

pub fn save_secret(bucket: &str, secret: &str) -> Result<()> {
    Ok(entry(bucket)?.set_password(secret)?)
}

pub fn load_secret(bucket: &str) -> Result<String> {
    Ok(entry(bucket)?.get_password()?)
}

pub fn delete_secret(bucket: &str) {
    if let Ok(entry) = entry(bucket) {
        let _ = entry.delete_credential();
    }
}

// ---- operations --------------------------------------------------------------------------

/// A folder (`size` is `None`) or file in a listing; `modified` is unix seconds.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub size: Option<u64>,
    pub modified: i64,
}

/// `Path::from` would percent-encode non-ASCII names into literal `%C3%A9` keys; `parse` keeps
/// them as typed and only fails on control characters, `.` and `..`, where encoding is right.
pub fn key(s: &str) -> Path {
    Path::parse(s).unwrap_or_else(|_| Path::from(s))
}

pub fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// `prefix` is "" or "a/b/".
pub(crate) fn prefix_path(prefix: &str) -> Option<Path> {
    (!prefix.trim_matches('/').is_empty()).then(|| key(prefix))
}

pub async fn list_dir(store: &dyn ObjectStore, prefix: &str) -> Result<Vec<Entry>> {
    let listing = store.list_with_delimiter(prefix_path(prefix).as_ref()).await?;
    let folders = listing.common_prefixes.iter().filter_map(|p| {
        Some(Entry { name: p.filename()?.to_string(), size: None, modified: 0 })
    });
    let files = listing.objects.iter().filter_map(|o| {
        Some(Entry {
            name: o.location.filename()?.to_string(),
            size: Some(o.size as u64),
            modified: o.last_modified.timestamp(),
        })
    });
    Ok(folders.chain(files).collect())
}

/// Small files go up as one PUT; bigger ones stream in 8 MiB parts, at most 4 in flight.
/// Returns the new ETag.
pub async fn upload(
    store: &dyn ObjectStore,
    local: &std::path::Path,
    key: &Path,
    attributes: Attributes,
    on_progress: impl Fn(u64),
) -> Result<Option<String>> {
    let mut file = tokio::fs::File::open(local).await?;
    let len = file.metadata().await?.len();
    if len <= PART as u64 {
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf).await?;
        let put = store.put_opts(key, PutPayload::from(buf), PutOptions { attributes, ..Default::default() }).await?;
        on_progress(len);
        return Ok(put.e_tag);
    }
    let mut write = start_multipart(store, key, attributes).await?;
    let mut buf = vec![0u8; 1 << 20];
    let mut sent = 0;
    let copied: Result<()> = async {
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            write.wait_for_capacity(4).await?;
            write.write(&buf[..n]);
            sent += n as u64;
            on_progress(sent);
        }
    }
    .await;
    Ok(finish_multipart(write, copied).await?.e_tag)
}

pub(crate) async fn start_multipart(store: &dyn ObjectStore, key: &Path, attributes: Attributes) -> Result<WriteMultipart> {
    let upload = store.put_multipart_opts(key, PutMultipartOptions { attributes, ..Default::default() }).await?;
    Ok(WriteMultipart::new_with_chunk_size(upload, PART))
}

/// Completes the upload, or aborts it (no orphaned parts) when `copied` failed.
pub(crate) async fn finish_multipart(write: WriteMultipart, copied: Result<()>) -> Result<PutResult> {
    match copied {
        Ok(()) => Ok(write.finish().await?),
        Err(error) => {
            let _ = write.abort().await;
            Err(error)
        }
    }
}

pub async fn download(store: &dyn ObjectStore, key: &Path, dest: &std::path::Path) -> Result<()> {
    let mut stream = store.get(key).await?.into_stream();
    let mut out = tokio::fs::File::create(dest).await?;
    while let Some(chunk) = stream.try_next().await? {
        out.write_all(&chunk).await?;
    }
    Ok(out.flush().await?)
}

/// Deletes one object, or everything under `key/` when `folder`.
pub async fn delete(store: &dyn ObjectStore, key: &Path, folder: bool) -> Result<()> {
    if !folder {
        return Ok(store.delete(key).await?);
    }
    let keys = store.list(Some(key)).map_ok(|meta| meta.location).boxed();
    store.delete_stream(keys).try_collect::<Vec<_>>().await?;
    Ok(())
}

pub async fn rename(store: &dyn ObjectStore, from: &Path, to: &Path) -> Result<()> {
    Ok(store.rename(from, to).await?)
}

/// A GET link that works for 7 days, the SigV4 maximum.
pub async fn share_url(store: &AmazonS3, key: &Path) -> Result<String> {
    let url = store
        .signed_url(http::Method::GET, key, Duration::from_secs(7 * 86400))
        .await?;
    Ok(url.to_string())
}

// ---- dropped files -> keys ---------------------------------------------------------------

/// Where a dropped `file` lands. The tree is kept relative to the dropped item (dropping
/// folder `photos` gives `<prefix>photos/2024/a.jpg`), names are NFC, and a name that is
/// taken becomes `name (1).ext`, `name (2).ext`, ...
pub fn key_for(
    dropped_root: &std::path::Path,
    file: &std::path::Path,
    current_prefix: &str,
    existing_keys: &HashSet<String>,
) -> Path {
    let base = dropped_root.parent().unwrap_or(dropped_root);
    let rel = file.strip_prefix(base).unwrap_or(file);
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().nfc().collect())
        .collect();
    let name = parts.pop().unwrap_or_default();
    let dir: String = parts.iter().map(|p| format!("{p}/")).collect();
    // A leading dot is part of the name (".env"), not an extension.
    let dot = name.rfind('.').filter(|&i| i > 0 && i + 1 < name.len());
    let (stem, ext) = name.split_at(dot.unwrap_or(name.len()));
    let candidate = |n: usize| match n {
        0 => key(&format!("{current_prefix}{dir}{name}")),
        n => key(&format!("{current_prefix}{dir}{stem} ({n}){ext}")),
    };
    (0..)
        .map(candidate)
        .find(|key| !existing_keys.contains(key.as_ref()))
        .expect("an unbounded range")
}

/// A file to upload and where it goes.
pub struct Planned {
    pub local: PathBuf,
    pub key: Path,
    pub size: u64,
}

fn walk(path: &std::path::Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        for item in std::fs::read_dir(path)? {
            walk(&item?.path(), out)?;
        }
    } else if path.file_name().is_some_and(|n| n != ".DS_Store") {
        out.push(path.to_path_buf());
    }
    Ok(())
}

/// Walks the dropped items and picks a key for every file under `prefix`. With `encrypt` the
/// keys get a `.age` suffix, and only other `.age` objects count as taken.
pub async fn plan(store: &dyn ObjectStore, roots: &[PathBuf], prefix: &str, encrypt: bool) -> Result<Vec<Planned>> {
    // Keys that could collide: this folder's files, and everything under dropped folders.
    // Stored without `.age` when encrypting, which is what `key_for` compares.
    let taken = |key: &Path| {
        let key = key.to_string();
        if !encrypt {
            return Some(key);
        }
        key.strip_suffix(crypt::SUFFIX).map(str::to_owned)
    };
    let mut existing: HashSet<String> = store
        .list_with_delimiter(prefix_path(prefix).as_ref())
        .await?
        .objects
        .iter()
        .filter_map(|o| taken(&o.location))
        .collect();
    for root in roots.iter().filter(|r| r.is_dir()) {
        let name = root.file_name().context("dropped folder has no name")?;
        let folder = key(&format!("{prefix}{}", name.to_string_lossy().nfc().collect::<String>()));
        let mut keys = store.list(Some(&folder));
        while let Some(meta) = keys.try_next().await? {
            existing.extend(taken(&meta.location));
        }
    }
    let mut planned = Vec::new();
    for root in roots {
        let mut files = Vec::new();
        walk(root, &mut files)?;
        for local in files {
            let mut key = key_for(root, &local, prefix, &existing);
            existing.insert(key.to_string());
            if encrypt {
                key = self::key(&crypt::add_age(key.as_ref()));
            }
            let size = std::fs::metadata(&local)?.len();
            planned.push(Planned { local, key, size });
        }
    }
    Ok(planned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(root: &str, file: &str, prefix: &str, taken: &[&str]) -> String {
        let taken = taken.iter().map(|s| s.to_string()).collect();
        key_for(root.as_ref(), file.as_ref(), prefix, &taken).to_string()
    }

    #[test]
    fn collisions_count_up_and_keep_extensions() {
        let taken = ["r.pdf", "r (1).pdf", ".env", "Makefile"];
        assert_eq!(key("/d/r.pdf", "/d/r.pdf", "", &taken), "r (2).pdf");
        assert_eq!(key("/d/.env", "/d/.env", "", &taken), ".env (1)");
        assert_eq!(key("/d/Makefile", "/d/Makefile", "", &taken), "Makefile (1)");
        assert_eq!(key("/d/new.txt", "/d/new.txt", "", &taken), "new.txt");
    }

    #[test]
    fn errors_get_short_messages() {
        let e = |s: &str| friendly(&anyhow::anyhow!("Generic S3 error: {s}"));
        assert_eq!(e("Error performing GET: 403 Forbidden AccessDenied"), "Access denied — check keys");
        assert_eq!(e("<Code>NoSuchBucket</Code>"), "Bucket not found — check the bucket name");
        assert_eq!(e("error sending request for url (https://x)"), "Can't reach bucket — check endpoint");
        assert_eq!(e("weird"), "Generic S3 error: weird");
    }

    #[test]
    fn names_are_nfc() {
        assert_eq!(key("/d/e\u{301}.txt", "/d/e\u{301}.txt", "", &[]), "\u{e9}.txt");
    }

    #[test]
    fn folders_keep_their_tree_under_the_prefix() {
        assert_eq!(
            key("/x/photos", "/x/photos/2024/a.jpg", "docs/", &[]),
            "docs/photos/2024/a.jpg"
        );
        assert_eq!(
            key("/x/photos", "/x/photos/2024/a.jpg", "docs/", &["docs/photos/2024/a.jpg"]),
            "docs/photos/2024/a (1).jpg"
        );
    }

    #[tokio::test]
    async fn encrypted_plan_only_collides_with_age_objects() {
        use object_store::memory::InMemory;
        let store = InMemory::new();
        for k in ["r.pdf", "s.pdf.age"] {
            store.put(&Path::from(k), PutPayload::from("x")).await.unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = ["r.pdf", "s.pdf"].map(|n| dir.path().join(n)).into();
        for f in &files {
            std::fs::write(f, "x").unwrap();
        }
        let keys = |planned: Vec<Planned>| planned.iter().map(|p| p.key.to_string()).collect::<Vec<_>>();
        assert_eq!(keys(plan(&store, &files, "", true).await.unwrap()), ["r.pdf.age", "s (1).pdf.age"]);
        assert_eq!(keys(plan(&store, &files, "", false).await.unwrap()), ["r (1).pdf", "s.pdf"]);
    }
}
