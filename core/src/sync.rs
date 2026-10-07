//! Two-way folder <-> bucket-prefix sync (BLUEPRINT §8). Three-way merge against a base snapshot.
use anyhow::Result;
use futures::TryStreamExt;
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode};
use object_store::{path::Path, Attribute, AttributeValue, Attributes, GetOptions, ObjectStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, io::Read, path::{Path as FsPath, PathBuf}, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};
use tokio::io::AsyncWriteExt;

use crate::{nfc, prefix_path, upload as put};

pub type State = BTreeMap<String, Meta>;

const SHA_KEY: &str = "shelf-sha256";
const STABLE_WAIT: Duration = Duration::from_millis(300);

/// Local: size + mtime(ms) (etag empty). Remote: size + etag (mtime = last_modified).
/// Base: local size/mtime and remote etag as of the last successful sync.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Meta { pub size: u64, pub mtime: i64, pub etag: String }

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Action { Noop, Upload, Download, Conflict, DeleteRemote, DeleteLocal, Adopt, Forget }

/// The §8 decision table. ETags are only compared to each other (change tokens), never as hashes.
pub fn decide(local: Option<Meta>, remote: Option<Meta>, base: Option<Meta>) -> Action {
    use Action::*;
    match (local, remote, base) {
        (None, None, None) => Noop,
        (Some(_), None, None) => Upload,
        (None, Some(_), None) => Download,
        (Some(l), Some(r), None) => if l.size == r.size && l.etag == r.etag { Adopt } else { Conflict },
        (None, None, Some(_)) => Forget,
        (None, Some(r), Some(b)) => if r.etag == b.etag { DeleteRemote } else { Download },
        (Some(l), None, Some(b)) => if l.size == b.size && l.mtime == b.mtime { DeleteLocal } else { Upload },
        (Some(l), Some(r), Some(b)) => match (l.size != b.size || l.mtime != b.mtime, r.etag != b.etag) {
            (false, false) => Noop,
            (true, false) => Upload,
            (false, true) => Download,
            (true, true) => Conflict,
        },
    }
}

pub fn load_state(p: &FsPath) -> Result<State> {
    match std::fs::read(p) { Ok(b) => Ok(serde_json::from_slice(&b)?), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::new()), Err(e) => Err(e.into()) }
}

pub fn save_state(p: &FsPath, s: &State) -> Result<()> {
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    Ok(std::fs::rename(tmp, p)?)
}

fn ignored(n: &str) -> bool {
    matches!(n, ".DS_Store" | ".Spotlight-V100") || n.starts_with("._") || n.starts_with("~$") || n.starts_with(".shelf-tmp") || n.ends_with(".icloud") || n.ends_with(".swp")
}

fn stat(p: &FsPath) -> Result<Meta> {
    let m = std::fs::metadata(p)?;
    let ms = m.modified()?.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    Ok(Meta { size: m.len(), mtime: ms, etag: String::new() })
}

fn scan(root: &FsPath, rel: &str, out: &mut BTreeMap<String, Meta>) -> Result<()> {
    for e in std::fs::read_dir(root.join(rel))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if ignored(&name) { continue }
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let t = e.file_type()?; // symlinks are skipped
        // Keys are NFC (like uploads); APFS finds the file under either form.
        if t.is_dir() { scan(root, &r, out)? } else if t.is_file() { out.insert(nfc(&r), stat(&e.path())?); }
    }
    Ok(())
}

/// `Path::from` would percent-encode non-ASCII names; `crate::key` parses them as they are.
fn key(prefix: &str, rel: &str) -> Path { crate::key(&if prefix.is_empty() { rel.to_string() } else { format!("{prefix}/{rel}") }) }

fn sha256_file(p: &FsPath) -> Result<String> {
    let (mut f, mut h, mut buf) = (std::fs::File::open(p)?, Sha256::new(), vec![0u8; 1 << 20]);
    loop { let n = f.read(&mut buf)?; if n == 0 { break } h.update(&buf[..n]) }
    Ok(format!("{:x}", h.finalize()))
}

async fn wait_stable(p: &FsPath) -> Result<()> {
    loop {
        let a = std::fs::metadata(p)?.len();
        tokio::time::sleep(STABLE_WAIT).await;
        if std::fs::metadata(p)?.len() == a { return Ok(()) }
    }
}

async fn upload(store: &dyn ObjectStore, f: &FsPath, key: &Path) -> Result<Meta> {
    wait_stable(f).await?;
    let mut m = stat(f)?; // stat before reading: if it changes meanwhile, next pass re-uploads
    let attributes = Attributes::from_iter([(Attribute::Metadata(SHA_KEY.into()), AttributeValue::from(sha256_file(f)?))]);
    m.etag = put(store, f, key, attributes, |_| {}).await?.unwrap_or_default();
    Ok(m)
}

async fn download(store: &dyn ObjectStore, key: &Path, dest: &FsPath) -> Result<Meta> {
    let res = store.get(key).await?;
    let etag = res.meta.e_tag.clone().unwrap_or_default();
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let tmp = dest.with_file_name(format!(".shelf-tmp-{nanos}"));
    let mut f = tokio::fs::File::create(&tmp).await?;
    let mut s = res.into_stream();
    while let Some(c) = s.try_next().await? { f.write_all(&c).await? }
    f.flush().await?;
    drop(f);
    std::fs::rename(&tmp, dest)?;
    Ok(Meta { etag, ..stat(dest)? })
}

/// `name (conflict DATE).ext`; `n` > 1 adds a counter: `name (conflict DATE 2).ext`.
fn conflict_name(rel: &str, date: &str, n: u32) -> String {
    let tag = if n > 1 { format!("{date} {n}") } else { date.to_string() };
    let i = rel.rfind('/').map_or(0, |i| i + 1);
    match rel[i..].rfind('.') {
        Some(d) if d > 0 => format!("{} (conflict {tag}){}", &rel[..i + d], &rel[i + d..]),
        _ => format!("{rel} (conflict {tag})"),
    }
}

/// The first conflict copy name that `taken` (local or remote) doesn't know: never overwrites one.
fn free_conflict_name(rel: &str, date: &str, taken: impl Fn(&str) -> bool) -> String {
    (1..).map(|n| conflict_name(rel, date, n)).find(|c| !taken(c)).expect("an unbounded range")
}

fn today() -> String { // UTC civil date from days since epoch (Hinnant)
    let z = (SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) / 86400) as i64 + 719468;
    let (era, doe) = (z.div_euclid(146097), z.rem_euclid(146097));
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + (m <= 2) as i64)
}

async fn remote_sha(store: &dyn ObjectStore, k: &Path) -> Option<String> {
    let r = store.get_opts(k, GetOptions { head: true, ..Default::default() }).await.ok()?;
    match r.attributes.get(&Attribute::Metadata(SHA_KEY.into()))? { v => Some(v.to_string()) }
}

/// One full pass: scan local, list remote, decide per path, apply, save the new base.
/// Per-file failures don't stop the pass; the first one is returned after state is saved.
pub async fn sync_once(store: &Arc<dyn ObjectStore>, prefix: &str, local_root: &FsPath, state_path: &FsPath) -> Result<()> {
    let store = &**store;
    let prefix = prefix.trim_matches('/');
    let mut base = load_state(state_path)?;
    let (mut local, mut remote) = (BTreeMap::new(), BTreeMap::new());
    scan(local_root, "", &mut local)?;
    let pfx = prefix_path(prefix);
    let mut list = store.list(pfx.as_ref());
    while let Some(o) = list.try_next().await? {
        let rel = o.location.as_ref().strip_prefix(prefix).unwrap_or(o.location.as_ref()).trim_start_matches('/').to_string();
        if rel.rsplit('/').next().is_some_and(ignored) { continue }
        remote.insert(rel, Meta { size: o.size as u64, mtime: o.last_modified.timestamp_millis(), etag: o.e_tag.unwrap_or_default() });
    }
    drop(list);

    let mut first_err = None;
    let rels: BTreeSet<_> = local.keys().chain(remote.keys()).chain(base.keys()).cloned().collect();
    for rel in rels {
        let (mut l, r, b) = (local.get(&rel).cloned(), remote.get(&rel).cloned(), base.get(&rel).cloned());
        let (k, p) = (key(prefix, &rel), local_root.join(&rel));
        // new/new: ETags can't be computed locally, so equal sha256 (object metadata) stands in for "same etag".
        if let (None, Some(lm), Some(rm)) = (&b, &mut l, &r) {
            if lm.size == rm.size && remote_sha(store, &k).await.is_some_and(|s| sha256_file(&p).is_ok_and(|h| h == s)) { lm.etag = rm.etag.clone() }
        }
        let a = decide(l.clone(), r.clone(), b);
        let res: Result<()> = async {
            match a {
                Action::Noop => {}
                Action::Forget => { base.remove(&rel); }
                Action::Adopt => { base.insert(rel.clone(), Meta { etag: r.unwrap().etag, ..l.unwrap() }); }
                Action::Upload => { base.insert(rel.clone(), upload(store, &p, &k).await?); }
                Action::Download => { base.insert(rel.clone(), download(store, &k, &p).await?); }
                Action::DeleteRemote => { store.delete(&k).await?; base.remove(&rel); }
                Action::DeleteLocal => { trash::delete(&p)?; base.remove(&rel); }
                Action::Conflict => {
                    // remote copy becomes the conflict copy (remote + local), then local wins the original name
                    let c = free_conflict_name(&rel, &today(), |c| local_root.join(c).exists() || remote.contains_key(c));
                    remote.insert(c.clone(), Meta::default()); // taken from now on
                    let ck = key(prefix, &c);
                    store.copy(&k, &ck).await?;
                    base.insert(c.clone(), download(store, &ck, &local_root.join(&c)).await?);
                    base.insert(rel.clone(), upload(store, &p, &k).await?);
                }
            }
            Ok(())
        }.await;
        if let Err(e) = res { first_err.get_or_insert(e); }
    }
    save_state(state_path, &base)?;
    first_err.map_or(Ok(()), Err)
}

/// Dropping the handle stops the watcher and the poll loop.
pub struct WatchHandle { task: tokio::task::JoinHandle<()>, _watcher: Box<dyn std::any::Any + Send> }
impl Drop for WatchHandle { fn drop(&mut self) { self.task.abort() } }

/// `watch` for a saved pair; its base lives in `pair.state_path()`.
pub fn watch_pair(store: crate::Store, pair: &crate::SyncPair) -> Result<WatchHandle> {
    watch(store, pair.prefix.clone(), PathBuf::from(&pair.folder), pair.state_path()?)
}

/// Debounced (2 s) local watcher + 60 s remote poll; each triggers `sync_once`. Call inside a tokio runtime.
pub fn watch(store: Arc<dyn ObjectStore>, prefix: String, local_root: PathBuf, state_path: PathBuf) -> Result<WatchHandle> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut deb = new_debouncer(Duration::from_secs(2), None, move |r: notify_debouncer_full::DebounceEventResult| { if r.is_ok() { let _ = tx.send(()); } })?;
    deb.watch(&local_root, RecursiveMode::Recursive)?;
    let task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60)); // first tick fires at once = initial sync
        loop {
            tokio::select! { _ = tick.tick() => {}, m = rx.recv() => if m.is_none() { break } }
            while rx.try_recv().is_ok() {} // coalesce queued events
            if let Err(e) = sync_once(&store, &prefix, &local_root, &state_path).await { eprintln!("shelf-sync: {e}") }
        }
    });
    Ok(WatchHandle { task, _watcher: Box::new(deb) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{memory::InMemory, PutPayload};

    fn m(size: u64, mtime: i64, etag: &str) -> Meta { Meta { size, mtime, etag: etag.into() } }
    fn base() -> Option<Meta> { Some(m(1, 10, "e1")) }
    fn same_l() -> Option<Meta> { Some(m(1, 10, "")) }
    fn chg_l() -> Option<Meta> { Some(m(2, 11, "")) }
    fn same_r() -> Option<Meta> { Some(m(1, 99, "e1")) }
    fn chg_r() -> Option<Meta> { Some(m(3, 99, "e2")) }

    #[test]
    fn decision_table() {
        use Action::*;
        assert_eq!(decide(chg_l(), same_r(), base()), Upload);
        assert_eq!(decide(same_l(), chg_r(), base()), Download);
        assert_eq!(decide(chg_l(), chg_r(), base()), Conflict);
        assert_eq!(decide(None, same_r(), base()), DeleteRemote);
        assert_eq!(decide(same_l(), None, base()), DeleteLocal);
        assert_eq!(decide(None, chg_r(), base()), Download);
        // new/new
        assert_eq!(decide(Some(m(5, 1, "x")), Some(m(5, 2, "x")), None), Adopt);
        assert_eq!(decide(Some(m(5, 1, "")), Some(m(5, 2, "x")), None), Conflict);
        assert_eq!(decide(Some(m(5, 1, "x")), Some(m(6, 2, "x")), None), Conflict);
        // remaining cells
        assert_eq!(decide(same_l(), same_r(), base()), Noop);
        assert_eq!(decide(chg_l(), None, base()), Upload);
        assert_eq!(decide(None, None, base()), Forget);
        assert_eq!(decide(chg_l(), None, None), Upload);
        assert_eq!(decide(None, same_r(), None), Download);
        assert_eq!(decide(None, None, None), Noop);
    }

    #[test]
    fn names_and_ignores() {
        assert_eq!(conflict_name("a/b.tar.gz", "2026-10-07", 1), "a/b.tar (conflict 2026-10-07).gz");
        assert_eq!(conflict_name("a.d/README", "2026-10-07", 1), "a.d/README (conflict 2026-10-07)");
        assert_eq!(conflict_name(".env", "2026-10-07", 1), ".env (conflict 2026-10-07)");
        assert_eq!(conflict_name("n.txt", "d", 1), "n (conflict d).txt");
        assert_eq!(conflict_name("a/n.txt", "d", 2), "a/n (conflict d 2).txt");
        assert_eq!(conflict_name("n", "d", 3), "n (conflict d 3)");
        // never an existing name, local or remote: counts up to the first free one
        let taken = ["n (conflict d).txt", "n (conflict d 2).txt"];
        assert_eq!(free_conflict_name("n.txt", "d", |c| taken.contains(&c)), "n (conflict d 3).txt");
        assert_eq!(free_conflict_name("m.txt", "d", |c| taken.contains(&c)), "m (conflict d).txt");
        for n in [".DS_Store", "._x", ".Spotlight-V100", "a.icloud", "~$d.docx", "a.swp", ".shelf-tmp-1"] { assert!(ignored(n), "{n}") }
        assert!(!ignored("a.txt"));
        assert_eq!(today().len(), 10);
    }

    #[tokio::test]
    async fn sync_once_end_to_end() {
        let (root, st) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (state, r) = (st.path().join("s.json"), root.path());
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // upload local-new, download remote-new, ignore junk
        std::fs::create_dir(r.join("d")).unwrap();
        std::fs::write(r.join("d/a.txt"), "aaa").unwrap();
        std::fs::write(r.join(".DS_Store"), "x").unwrap();
        store.put(&Path::from("pfx/b.txt"), PutPayload::from("bbbb")).await.unwrap();
        sync_once(&store, "pfx", r, &state).await.unwrap();
        assert_eq!(store.get(&Path::from("pfx/d/a.txt")).await.unwrap().bytes().await.unwrap().as_ref(), b"aaa");
        assert!(store.head(&Path::from("pfx/.DS_Store")).await.is_err());
        let attrs = store.get(&Path::from("pfx/d/a.txt")).await.unwrap().attributes;
        assert_eq!(attrs.get(&Attribute::Metadata(SHA_KEY.into())).unwrap().as_ref(), format!("{:x}", Sha256::digest(b"aaa")));
        assert_eq!(std::fs::read_to_string(r.join("b.txt")).unwrap(), "bbbb");
        assert_eq!(std::fs::read_dir(r).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(".shelf-tmp")).count(), 0);
        assert_eq!(load_state(&state).unwrap().len(), 2);

        // second pass is a no-op
        let before = load_state(&state).unwrap();
        sync_once(&store, "pfx", r, &state).await.unwrap();
        assert_eq!(load_state(&state).unwrap(), before);

        // both sides change b.txt -> conflict copy on both sides, local wins the original name
        std::fs::write(r.join("b.txt"), "local-new").unwrap();
        store.put(&Path::from("pfx/b.txt"), PutPayload::from("remote!")).await.unwrap();
        sync_once(&store, "pfx", r, &state).await.unwrap();
        let c = conflict_name("b.txt", &today(), 1);
        assert_eq!(std::fs::read_to_string(r.join(&c)).unwrap(), "remote!");
        assert_eq!(store.get(&Path::from(format!("pfx/{c}"))).await.unwrap().bytes().await.unwrap().as_ref(), b"remote!");
        assert_eq!(store.get(&Path::from("pfx/b.txt")).await.unwrap().bytes().await.unwrap().as_ref(), b"local-new");

        // a second conflict the same day must not overwrite the first copy
        std::fs::write(r.join("b.txt"), "local-newer!").unwrap();
        store.put(&Path::from("pfx/b.txt"), PutPayload::from("remote again")).await.unwrap();
        sync_once(&store, "pfx", r, &state).await.unwrap();
        let c2 = conflict_name("b.txt", &today(), 2);
        assert_eq!(std::fs::read_to_string(r.join(&c)).unwrap(), "remote!");
        assert_eq!(std::fs::read_to_string(r.join(&c2)).unwrap(), "remote again");
        assert_eq!(store.get(&Path::from(format!("pfx/{c}"))).await.unwrap().bytes().await.unwrap().as_ref(), b"remote!");
        assert_eq!(store.get(&Path::from(format!("pfx/{c2}"))).await.unwrap().bytes().await.unwrap().as_ref(), b"remote again");
        assert_eq!(store.get(&Path::from("pfx/b.txt")).await.unwrap().bytes().await.unwrap().as_ref(), b"local-newer!");

        // local file removed (unchanged remote) -> remote delete
        std::fs::remove_file(r.join("d/a.txt")).unwrap();
        sync_once(&store, "pfx", r, &state).await.unwrap();
        assert!(store.head(&Path::from("pfx/d/a.txt")).await.is_err());
        assert!(!load_state(&state).unwrap().contains_key("d/a.txt"));

        // new/new with identical content (sha256 metadata) is adopted, not conflicted
        let (root2, st2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(root2.path().join("x.txt"), "hi").unwrap();
        sync_once(&store, "p2", root2.path(), &st2.path().join("s.json")).await.unwrap(); // uploads with sha
        let lost = st2.path().join("lost.json"); // fresh state: x.txt is new on both sides
        sync_once(&store, "p2", root2.path(), &lost).await.unwrap();
        assert_eq!(std::fs::read_dir(root2.path()).unwrap().count(), 1);
        assert!(load_state(&lost).unwrap().contains_key("x.txt"));
    }

    #[tokio::test]
    async fn non_ascii_names_are_not_double_encoded() {
        let (root, st) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        std::fs::write(root.path().join("e\u{301}t\u{e9}.txt"), "x").unwrap(); // NFD on disk
        for (prefix, state) in [("pfx", "a.json"), ("", "b.json")] {
            sync_once(&store, prefix, root.path(), &st.path().join(state)).await.unwrap();
            let want = crate::key(&format!("{prefix}/\u{e9}t\u{e9}.txt"));
            assert!(store.head(&want).await.is_ok(), "{want}");
        }
        let keys: Vec<String> = store.list(None).map_ok(|m| m.location.to_string()).try_collect().await.unwrap();
        assert!(keys.iter().all(|k| !k.contains('%')), "{keys:?}");
    }
}
