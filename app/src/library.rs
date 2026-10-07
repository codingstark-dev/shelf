use std::{
    collections::HashSet,
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use ely_gpui_component::{
    buttons::{Button, ButtonVariant, IconButton, SegmentedControl},
    feedback::{ConfirmationCard, Toast, ToastViewport, Toaster},
    files::{Transfer, TransferQueue, TransferState},
    forms::{Input, InputEvent, SearchInput, Switch, TextInput},
    layout::Sheet,
    menus::{ContextMenu, Menu, MenuItem},
    motion::Skeleton,
    navigation::{Breadcrumb, Crumb},
    primitives::{IconName, Place, Severity},
    shell::{UpdateBanner, UpdateState, drag_region},
    theme::{ActiveTheme, ControlSize, Radius},
    typography::Paragraph,
};
use futures::{StreamExt, future::Either};
use gpui::{
    AnyElement, App, AppContext, ClickEvent, ClipboardItem, Context, DragMoveEvent, ElementId, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, PathPromptOptions, Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use jiff::Timestamp;
use shelf_core::{BucketConfig, Entry, Store, SyncPair, crypt, login, mount, update};

use crate::{
    Bg, Rt,
    ui::{self, FileRowData, PillKind, TrayState},
};

/// GitHub "<owner>/<repo>" the update check asks for the latest release.
const REPO: &str = "codingstark-dev/shelf";

/// The user pressed Disconnect; credentials are gone.
pub struct Disconnected;

enum Panel {
    Settings,
    Rename(String),
    Delete(String),
    /// The age recovery text, shown right after encryption is turned on.
    Recovery(String),
    Disconnect,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    List,
    Grid,
}

/// Where the column header starts and ends; matches `ui::file_row` (4 inset + 10 padding, 24 icon, 12 gap).
const HEADER_PAD: (f32, f32) = (50.0, 14.0);

pub struct Library {
    cfg: BucketConfig,
    secret: String,
    store: Store,
    prefix: String,
    entries: Vec<Entry>,
    /// Shown name of the selected entry (see `shown`).
    selected: Option<SharedString>,
    loading: bool,
    /// A short message and the full error text behind the "Details" toggle.
    error: Option<(SharedString, SharedString)>,
    show_details: bool,
    mounting: bool,
    update: Option<update::Release>,
    transfers: Arc<Mutex<Vec<Transfer>>>,
    toaster: Entity<Toaster>,
    panel: Option<Panel>,
    rename: Entity<TextInput>,
    _upload: Option<Task<()>>,
    view: View,
    /// Prefixes to go back to, and to go forward to after going back.
    history: Vec<String>,
    forward: Vec<String>,
    query: Entity<TextInput>,
    search_open: bool,
    /// Names of the last 5 finished uploads, newest first.
    recent_uploads: Vec<SharedString>,
    /// What the tray was last built for; it only rebuilds when this changes.
    tray_seen: (TrayState, Vec<SharedString>),
    show_transfers: bool,
    /// A file drag is inside the window.
    over: bool,
    /// A request to open the context menu: (number, where).
    menu_at: Option<(u64, Point<Pixels>)>,
    sync_sel: Option<usize>,
    focus: FocusHandle,
    _search: Subscription,
}

impl EventEmitter<Disconnected> for Library {}

/// How a listed name looks: encrypted files lose `.age` (the row adds the lock).
fn shown(entry: &Entry) -> SharedString {
    match entry.size {
        Some(_) if crypt::is_encrypted(&entry.name) => crypt::strip_age(&entry.name).to_string().into(),
        _ => entry.name.clone().into(),
    }
}

/// "12.4 MB", decimal like Finder.
fn bytes(n: u64) -> String {
    let (mut v, mut unit) = (n as f64, 0);
    while v >= 1000.0 && unit < 3 {
        (v, unit) = (v / 1000.0, unit + 1);
    }
    match unit {
        0 => format!("{n} B"),
        _ => format!("{v:.1} {}", ["B", "KB", "MB", "GB"][unit]),
    }
}

/// A headline and hint for a friendly error line (core's `friendly`); the raw text stays behind Details.
fn error_copy(short: &str) -> (&'static str, SharedString) {
    if short.starts_with("Access denied") {
        ("Access denied", "Check that your API token can read and write this bucket.".into())
    } else if short.starts_with("Bucket not found") {
        ("Bucket not found", "Check the bucket name.".into())
    } else if short.starts_with("Can't reach") {
        ("Can't reach your bucket", "Check your internet connection or the endpoint.".into())
    } else {
        ("Something went wrong", short.to_string().into())
    }
}

/// `text` with long unbreakable words cut, so keys wrap inside a narrow sheet.
fn wrapped(text: &str) -> String {
    let words = |line: &str| {
        line.split(' ')
            .map(|w| w.as_bytes().chunks(28).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("\n"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    text.lines().map(words).collect::<Vec<_>>().join("\n")
}

fn set(shared: &Mutex<Vec<Transfer>>, key: &str, change: impl FnOnce(&mut Transfer)) {
    if let Some(transfer) = shared.lock().unwrap().iter_mut().find(|t| &*t.key == key) {
        change(transfer);
    }
}

impl Library {
    pub fn new(cfg: BucketConfig, secret: &str, window: &mut Window, cx: &mut App) -> Result<Entity<Self>> {
        let store = shelf_core::open(&cfg, secret)?;
        let rename = cx.new(|cx| TextInput::new(window, cx));
        let query = cx.new(|cx| TextInput::new(window, cx).placeholder("Search"));
        let toaster = cx.new(|_| Toaster::default());
        crate::start_syncs_once(&cfg, secret, cx);
        Ok(cx.new(|cx| {
            let mut this = Self {
                cfg,
                secret: secret.to_string(),
                store,
                prefix: String::new(),
                entries: Vec::new(),
                selected: None,
                loading: false,
                error: None,
                show_details: false,
                mounting: false,
                update: None,
                transfers: Default::default(),
                toaster,
                panel: None,
                rename,
                _upload: None,
                view: View::List,
                history: Vec::new(),
                forward: Vec::new(),
                _search: cx.subscribe(&query, |_, _, _: &InputEvent, cx| cx.notify()),
                query,
                search_open: false,
                recent_uploads: Vec::new(),
                tray_seen: (TrayState::Idle, Vec::new()),
                show_transfers: false,
                over: false,
                menu_at: None,
                sync_sel: None,
                focus: cx.focus_handle(),
            };
            this.refresh(cx);
            this.check_for_updates(cx);
            this
        }))
    }

    /// At most once a day, and only if the user left it on.
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let Ok(dir) = shelf_core::support_dir() else { return };
        let file = dir.join("update-check");
        if !self.cfg.check_updates || std::fs::create_dir_all(&dir).is_err() || !update::due(&file) {
            return;
        }
        self.run(cx, update::check(REPO, env!("CARGO_PKG_VERSION")), |this, result, cx| match result {
            Ok(release) => {
                this.update = release;
                cx.notify();
            }
            Err(error) => log::warn!("update check: {error:#}"),
        });
    }

    /// Runs `work` on tokio and hands its result back on the UI thread.
    fn run<T: Send + 'static>(
        &self,
        cx: &mut Context<Self>,
        work: impl Future<Output = Result<T>> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T>, &mut Context<Self>) + 'static,
    ) {
        let job = cx.global::<Rt>().0.spawn(work);
        cx.spawn(async move |this, cx| {
            let result = job.await.map_err(anyhow::Error::from).and_then(|r| r);
            this.update(cx, |this, cx| done(this, result, cx)).ok();
        })
        .detach();
    }

    fn toast(&self, title: &str, body: impl Into<SharedString>, severity: Severity, cx: &mut Context<Self>) {
        let toast = Toast::new(title.to_string()).body(body).severity(severity);
        self.toaster.update(cx, |toaster, cx| toaster.push(toast, cx));
    }

    fn key(&self, name: &str) -> shelf_core::Path {
        shelf_core::key(&format!("{}{name}", self.prefix))
    }

    fn current(&self) -> Option<&Entry> {
        let name = self.selected.as_ref()?;
        self.entries.iter().find(|e| shown(e) == *name)
    }

    fn save(&self, cx: &mut Context<Self>) {
        if let Err(error) = shelf_core::save_config(&self.cfg) {
            self.toast("Couldn't save settings", error.to_string(), Severity::Danger, cx);
        }
    }

    /// The age identity for this bucket, or a toast saying why not.
    fn identity(&self, cx: &mut Context<Self>) -> Option<crypt::x25519::Identity> {
        match crypt::load_identity(&self.cfg.bucket) {
            Ok(Some(id)) => Some(id),
            Ok(None) => {
                self.toast("No recovery key", "This Mac has no key for this bucket's encrypted files.", Severity::Danger, cx);
                None
            }
            Err(error) => {
                self.toast("Keychain error", error.to_string(), Severity::Danger, cx);
                None
            }
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        let (store, prefix) = (self.store.clone(), self.prefix.clone());
        let asked = prefix.clone();
        self.run(cx, async move { shelf_core::list_dir(&*store, &prefix).await }, move |this, result, cx| {
            if this.prefix != asked {
                return; // navigated away meanwhile
            }
            this.loading = false;
            match result {
                Ok(entries) => (this.entries, this.error) = (entries, None),
                Err(error) => {
                    this.error = Some((shelf_core::friendly(&error).into(), format!("{error:#}").into()));
                    this.show_details = false;
                }
            }
            this.sync_tray(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Opens `prefix`, remembering where we were.
    fn go(&mut self, prefix: String, cx: &mut Context<Self>) {
        self.history.push(std::mem::take(&mut self.prefix));
        self.forward.clear();
        self.visit(prefix, cx);
    }

    fn visit(&mut self, prefix: String, cx: &mut Context<Self>) {
        (self.prefix, self.selected, self.entries) = (prefix, None, Vec::new());
        self.query.update(cx, |query, cx| query.set_text("", cx));
        self.refresh(cx);
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(prefix) = self.history.pop() {
            self.forward.push(self.prefix.clone());
            self.visit(prefix, cx);
        }
    }

    fn forward(&mut self, cx: &mut Context<Self>) {
        if let Some(prefix) = self.forward.pop() {
            self.history.push(self.prefix.clone());
            self.visit(prefix, cx);
        }
    }

    pub fn tray_state(&self) -> TrayState {
        let transfers = self.transfers.lock().unwrap();
        if transfers.iter().any(|t| matches!(t.state, TransferState::Queued | TransferState::Moving { .. })) {
            TrayState::Transferring
        } else if self.error.is_some() || transfers.iter().any(|t| matches!(t.state, TransferState::Failed(_))) {
            TrayState::Error
        } else {
            TrayState::Idle
        }
    }

    /// The last 5 finished uploads, newest first.
    pub fn recent(&self) -> Vec<SharedString> {
        self.recent_uploads.clone()
    }

    /// Rebuilds the tray, but only when its icon or recent list would change.
    fn sync_tray(&mut self, cx: &mut Context<Self>) {
        let now = (self.tray_state(), self.recent());
        if now != self.tray_seen {
            cx.global_mut::<crate::Bg>().tray = Some(now.clone());
            self.tray_seen = now;
            cx.defer(|cx| crate::tray(cx));
        }
    }

    fn open_entry(&mut self, name: &str, folder: bool, cx: &mut Context<Self>) {
        if folder {
            self.go(format!("{}{name}/", self.prefix), cx);
        } else {
            self.selected = Some(name.to_string().into());
            self.download(cx);
        }
    }

    // ---- actions on the selected entry ----------------------------------------------------

    fn share(&mut self, cx: &mut Context<Self>) {
        // A link to ciphertext would be useless, so encrypted files have no share action.
        let Some(entry) = self.current().filter(|e| e.size.is_some() && !crypt::is_encrypted(&e.name)) else { return };
        let (store, key) = (self.store.clone(), self.key(&entry.name));
        self.run(cx, async move { shelf_core::share_url(&store, &key).await }, |this, result, cx| match result {
            Ok(url) => {
                cx.write_to_clipboard(ClipboardItem::new_string(url));
                this.toast("Link copied", "Anyone with it can download for 7 days.", Severity::Success, cx);
            }
            Err(error) => this.toast("Couldn't create link", shelf_core::friendly(&error), Severity::Danger, cx),
        });
    }

    fn download(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.current().filter(|e| e.size.is_some()) else { return };
        let Some(home) = std::env::var_os("HOME") else { return };
        let (name, key, encrypted) = (entry.name.clone(), self.key(&entry.name), crypt::is_encrypted(&entry.name));
        let dest = PathBuf::from(home).join("Downloads").join(crypt::strip_age(&name));
        let identity = if encrypted {
            match self.identity(cx) {
                Some(id) => Some(id),
                None => return,
            }
        } else {
            None
        };
        let store = self.store.clone();
        self.toast("Downloading", name, Severity::Info, cx);
        self.run(
            cx,
            async move {
                match identity {
                    Some(id) => crypt::download_decrypted(&*store, &key, &dest, id).await,
                    None => shelf_core::download(&*store, &key, &dest).await,
                }
                .map(|()| dest)
            },
            |this, result, cx| match result {
                Ok(dest) => {
                    // Reveal, don't open: bucket contents aren't trusted, and opening could run a .app or .command.
                    let _ = std::process::Command::new("open").arg("-R").arg(&dest).spawn();
                    this.toast("Saved to Downloads", dest.display().to_string(), Severity::Success, cx);
                }
                Err(error) => this.toast("Download failed", shelf_core::friendly(&error), Severity::Danger, cx),
            },
        );
    }

    fn ask_rename(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.current().filter(|e| e.size.is_some()) else { return };
        let name = entry.name.clone();
        let text = crypt::strip_age(&name).to_string();
        self.rename.update(cx, |input, cx| input.set_text(text, cx));
        self.panel = Some(Panel::Rename(name));
        cx.notify();
    }

    fn do_rename(&mut self, old: &str, cx: &mut Context<Self>) {
        let mut new = shelf_core::nfc(self.rename.read(cx).text().trim());
        if new.is_empty() || new.contains('/') || new == crypt::strip_age(old) {
            return self.toast("Pick another name", "It must be new, non-empty, and have no “/”.", Severity::Warning, cx);
        }
        if crypt::is_encrypted(old) {
            new = crypt::add_age(&new); // stays encrypted, stays marked
        }
        if self.entries.iter().any(|e| e.name == new) {
            return self.toast("Name taken", new, Severity::Warning, cx);
        }
        let (store, from, to) = (self.store.clone(), self.key(old), self.key(&new));
        self.panel = None;
        self.selected = None;
        self.run(cx, async move { shelf_core::rename(&*store, &from, &to).await }, |this, result, cx| {
            match result {
                Ok(()) => this.refresh(cx),
                Err(error) => this.toast("Rename failed", shelf_core::friendly(&error), Severity::Danger, cx),
            }
        });
    }

    fn ask_delete(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.current() {
            self.panel = Some(Panel::Delete(entry.name.clone()));
            cx.notify();
        }
    }

    fn do_delete(&mut self, name: &str, cx: &mut Context<Self>) {
        let folder = self.entries.iter().any(|e| e.name == name && e.size.is_none());
        let (store, key) = (self.store.clone(), self.key(name));
        self.panel = None;
        self.selected = None;
        self.run(cx, async move { shelf_core::delete(&*store, &key, folder).await }, |this, result, cx| {
            match result {
                Ok(()) => this.refresh(cx),
                Err(error) => this.toast("Delete failed", shelf_core::friendly(&error), Severity::Danger, cx),
            }
        });
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        let bg = cx.global_mut::<Bg>();
        bg.syncs.clear();
        drop(bg.mount.take());
        for pair in &self.cfg.syncs {
            if let Ok(path) = pair.state_path() {
                let _ = std::fs::remove_file(path);
            }
        }
        // clear_config below drops both login flags with the rest of the config.
        if let Err(error) = login::set_enabled(false) {
            log::warn!("start at login: {error:#}");
        }
        crate::tray(cx);
        // The age identity stays in the Keychain: deleting it would strand encrypted files.
        shelf_core::delete_secret(&self.cfg.bucket);
        shelf_core::clear_config();
        cx.emit(Disconnected);
    }

    // ---- settings --------------------------------------------------------------------------

    fn set_encrypt(&mut self, on: bool, cx: &mut Context<Self>) {
        if on {
            match crypt::load_identity(&self.cfg.bucket) {
                Ok(Some(_)) => self.toast("Using your existing recovery key", "It is already in the Keychain.", Severity::Info, cx),
                Ok(None) => {
                    let id = crypt::new_identity();
                    if let Err(error) = crypt::save_identity(&self.cfg.bucket, &id) {
                        return self.toast("Couldn't save the key", error.to_string(), Severity::Danger, cx);
                    }
                    self.panel = Some(Panel::Recovery(crypt::recovery_text(&id)));
                }
                Err(error) => return self.toast("Keychain error", error.to_string(), Severity::Danger, cx),
            }
            // Mount and sync would only see ciphertext.
            cx.global_mut::<Bg>().syncs.clear();
            self.eject(cx);
        }
        self.cfg.encrypt = on;
        self.save(cx);
        if !on {
            crate::start_syncs(&self.cfg, &self.secret, cx);
        }
        cx.notify();
    }

    fn set_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.cfg.check_updates = on;
        self.save(cx);
        cx.notify();
    }

    fn set_open_at_login(&mut self, on: bool, cx: &mut Context<Self>) {
        self.cfg.open_at_login = on;
        self.save(cx);
        if let Err(error) = login::set_enabled(on) {
            self.toast("Couldn't change login item", error.to_string(), Severity::Danger, cx);
        }
        cx.notify();
    }

    fn set_mount_at_login(&mut self, on: bool, cx: &mut Context<Self>) {
        self.cfg.mount_at_login = on;
        self.save(cx);
        cx.notify();
    }

    fn copy_recovery(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.identity(cx) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(crypt::recovery_text(&id)));
        self.toast("Copied", "Paste it somewhere safe.", Severity::Success, cx);
    }

    fn open_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.panel = Some(panel);
        cx.notify();
    }

    fn add_sync(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Sync".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            if let Some(folder) = paths.into_iter().next() {
                this.update(cx, |this, cx| this.start_pair(folder, cx)).ok();
            }
        })
        .detach();
    }

    /// Syncs `folder` with the folder the Library is showing.
    fn start_pair(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let pair = SyncPair { folder: folder.to_string_lossy().into_owned(), prefix: self.prefix.clone() };
        if self.cfg.syncs.contains(&pair) {
            return self.toast("Already syncing", pair.folder, Severity::Info, cx);
        }
        match crate::start_sync(&pair, &self.store, cx) {
            Ok(()) => {
                self.toast("Syncing", format!("{} with /{}", pair.folder, pair.prefix), Severity::Success, cx);
                self.cfg.syncs.push(pair);
                self.save(cx);
            }
            Err(error) => self.toast("Couldn't start sync", error.to_string(), Severity::Danger, cx),
        }
        cx.notify();
    }

    fn remove_sync(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.cfg.syncs.len() {
            return;
        }
        self.sync_sel = None;
        let pair = self.cfg.syncs.remove(index);
        cx.global_mut::<Bg>().syncs.retain(|(p, _)| *p != pair);
        if let Ok(path) = pair.state_path() {
            let _ = std::fs::remove_file(path);
        }
        self.save(cx);
        cx.notify();
    }

    // ---- Finder mount ----------------------------------------------------------------------

    pub fn toggle_mount(&mut self, cx: &mut Context<Self>) {
        if self.cfg.encrypt {
            return self.toast("Mount is off", "Finder would show ciphertext. Turn off encryption to mount.", Severity::Info, cx);
        }
        if self.mounting || cx.global::<Bg>().mounting {
            return;
        }
        if cx.global::<Bg>().mount.is_some() {
            self.cfg.mount_at_login = false;
            self.save(cx);
            return self.eject(cx);
        }
        if mount::find_rclone().is_none() {
            return self.toast("Couldn't mount", "rclone is not installed. Run: brew install rclone", Severity::Danger, cx);
        }
        let Some(dir) = crate::mountpoint(&self.cfg.bucket) else { return };
        let (cfg, secret) = (self.cfg.clone(), self.secret.clone());
        self.mounting = true;
        self.toast("Mounting", format!("~/Shelf/{}", cfg.bucket), Severity::Info, cx);
        let job = cx.global::<Rt>().0.spawn_blocking(move || {
            mount::mount(&cfg.endpoint, &cfg.bucket, &cfg.access_key_id, &secret, &dir)
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.map_err(anyhow::Error::from).and_then(|r| r);
            this.update(cx, |this, cx| {
                this.mounting = false;
                match result {
                    Ok(mount) => {
                        let _ = std::process::Command::new("open").arg(mount.mountpoint()).spawn();
                        cx.global_mut::<Bg>().mount = Some(mount);
                        this.cfg.mount_at_login = true;
                        this.save(cx);
                    }
                    Err(error) => this.toast("Couldn't mount", error.to_string(), Severity::Danger, cx),
                }
                crate::tray(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Unmounts and stops rclone (off the UI thread: `umount` can block).
    fn eject(&mut self, cx: &mut Context<Self>) {
        let Some(mount) = cx.global_mut::<Bg>().mount.take() else { return };
        let job = cx.global::<Rt>().0.spawn_blocking(move || drop(mount));
        cx.spawn(async move |this, cx| {
            let _ = job.await;
            this.update(cx, |_, cx| {
                crate::tray(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The empty state's click: pick files or folders to upload.
    fn choose(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: true, multiple: true, prompt: None });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            this.update(cx, |this, cx| this.enqueue(paths, cx)).ok();
        })
        .detach();
    }

    // ---- uploads ---------------------------------------------------------------------------

    fn enqueue(&mut self, roots: Vec<PathBuf>, cx: &mut Context<Self>) {
        let identity = match self.cfg.encrypt {
            true => match self.identity(cx) {
                Some(id) => Some(id),
                None => return,
            },
            false => None,
        };
        let (store, prefix, shared) = (self.store.clone(), self.prefix.clone(), self.transfers.clone());
        let job = cx.global::<Rt>().0.spawn(async move {
            let planned = shelf_core::plan(&*store, &roots, &prefix, identity.is_some()).await?;
            {
                let mut all = shared.lock().unwrap();
                all.retain(|t| matches!(t.state, TransferState::Queued | TransferState::Moving { .. }));
                all.extend(planned.iter().map(|p| Transfer {
                    key: p.key.to_string().into(),
                    name: crypt::strip_age(p.key.filename().unwrap_or_default()).to_string().into(),
                    upload: true,
                    moved: 0,
                    total: p.size,
                    state: TransferState::Queued,
                }));
            }
            // At most 3 uploads at once.
            let results = futures::stream::iter(planned)
                .map(|p| {
                    let (store, shared, identity) = (store.clone(), shared.clone(), identity.clone());
                    async move {
                        let key = p.key.to_string();
                        let name = crypt::strip_age(p.key.filename().unwrap_or_default()).to_string();
                        let started = Instant::now();
                        let progress = |sent: u64| {
                            let rate = (sent as f64 / started.elapsed().as_secs_f64().max(0.001)) as u64;
                            set(&shared, &key, |t| {
                                (t.moved, t.state) = (sent, TransferState::Moving { rate });
                            });
                        };
                        let result = match identity {
                            Some(id) => crypt::upload_encrypted(&*store, &p.local, &p.key, id, progress).await,
                            None => shelf_core::upload(&*store, &p.local, &p.key, Default::default(), progress).await.map(drop),
                        };
                        set(&shared, &key, |t| match &result {
                            Ok(()) => (t.moved, t.state) = (t.total, TransferState::Done),
                            Err(error) => t.state = TransferState::Failed(shelf_core::friendly(error).into()),
                        });
                        (result.is_ok(), name)
                    }
                })
                .buffer_unordered(3)
                .collect::<Vec<(bool, String)>>()
                .await;
            Ok(results)
        });
        self._upload = Some(cx.spawn(async move |this, cx| {
            // Progress arrives through the shared list; redraw it about every 100 ms.
            let mut job = job;
            let result = loop {
                let tick = cx.background_executor().timer(Duration::from_millis(100));
                match futures::future::select(job, tick).await {
                    Either::Left((result, _)) => break result,
                    Either::Right((_, pending)) => {
                        job = pending;
                        this.update(cx, |this, cx| {
                            this.sync_tray(cx);
                            cx.notify();
                        })
                        .ok();
                    }
                }
            };
            this.update(cx, |this, cx| {
                match result.map_err(anyhow::Error::from).and_then(|r| r) {
                    Ok(done) => {
                        for (_, name) in done.iter().filter(|(ok, _)| *ok) {
                            this.recent_uploads.insert(0, name.clone().into());
                        }
                        this.recent_uploads.truncate(5);
                        let failed = done.iter().filter(|(ok, _)| !ok).count();
                        if failed > 0 {
                            this.toast("Some uploads failed", format!("{failed} of {}", done.len()), Severity::Danger, cx);
                        } else if !done.is_empty() {
                            this.toast("Uploaded", format!("{} file(s)", done.len()), Severity::Success, cx);
                        }
                    }
                    Err(error) => this.toast("Upload failed", shelf_core::friendly(&error), Severity::Danger, cx),
                }
                this.sync_tray(cx);
                this.refresh(cx);
            })
            .ok();
        }));
    }
}

fn row_data(entry: &Entry) -> FileRowData {
    // ponytail: S3 has no folder mtime, so folders show "just now".
    let modified = match entry.size {
        Some(_) => Timestamp::from_second(entry.modified).unwrap_or(Timestamp::UNIX_EPOCH),
        None => Timestamp::now(),
    };
    FileRowData {
        name: shown(entry),
        folder: entry.size.is_none(),
        size: entry.size,
        modified,
        encrypted: entry.size.is_some() && crypt::is_encrypted(&entry.name),
    }
}

fn tool(id: impl Into<ElementId>, icon: IconName, tip: &'static str, size: ControlSize, run: impl Fn(&mut App) + 'static) -> IconButton {
    IconButton::new(id, icon)
        .variant(ButtonVariant::Ghost)
        .size(size)
        .tooltip(tip)
        .on_click(move |_, _, cx| run(cx))
}

impl Library {
    fn open_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.menu_at = Some((self.menu_at.map_or(1, |(n, _)| n + 1), at));
        cx.notify();
    }

    fn settings_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let (muted, subtle, danger, selection) = {
            let c = &cx.theme().colors;
            (c.fg_muted, c.fg_subtle, c.danger, c.selection)
        };
        let encrypt = self.cfg.encrypt;
        let radius = cx.theme().radius(Radius::Lg);
        let value = |text: String| {
            div().max_w(px(200.0)).overflow_hidden().whitespace_nowrap().text_ellipsis_middle().text_sm().text_color(muted).child(text).into_any_element()
        };
        let toggle = |id: &'static str, on: bool, f: fn(&mut Self, bool, &mut Context<Self>)| {
            let this = this.clone();
            Switch::new(id, on).on_change(move |on, _, cx| this.update(cx, |this, cx| f(this, on, cx))).into_any_element()
        };
        let key = self.cfg.access_key_id.clone();
        let key = if key.chars().count() > 8 {
            format!("{}…{}", key.chars().take(4).collect::<String>(), key.chars().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<String>())
        } else {
            key
        };

        let rclone = mount::find_rclone().is_some();
        let general = ui::settings_group(
            "g-general",
            Some("General"),
            (!rclone).then_some("Needs rclone (brew install rclone)"),
            vec![
                ui::settings_row(
                    "Open at login",
                    Switch::new("set-login", login::is_enabled())
                        .on_change({
                            let this = this.clone();
                            move |on, _, cx| this.update(cx, |this, cx| this.set_open_at_login(on, cx))
                        })
                        .into_any_element(),
                    cx,
                ),
                ui::settings_row(
                    "Show drive in Finder at login",
                    Switch::new("set-mount-login", rclone && self.cfg.mount_at_login)
                        .disabled(!rclone)
                        .on_change({
                            let this = this.clone();
                            move |on, _, cx| this.update(cx, |this, cx| this.set_mount_at_login(on, cx))
                        })
                        .into_any_element(),
                    cx,
                ),
            ],
            cx,
        );

        let bucket = ui::settings_group(
            "g-bucket",
            Some("Bucket"),
            None,
            vec![
                ui::settings_row("Bucket", value(self.cfg.bucket.clone()), cx),
                ui::settings_row("Endpoint", value(self.cfg.endpoint.clone()), cx),
                ui::settings_row("Access key ID", value(key), cx),
            ],
            cx,
        );

        let mut enc = vec![ui::settings_row("Encrypt new uploads", toggle("set-encrypt", encrypt, Self::set_encrypt), cx)];
        if encrypt {
            let copy = ui::pill_button("copy-recovery", "Copy", PillKind::Neutral, true, false, cx)
                .on_click(cx.listener(|this, _, _, cx| this.copy_recovery(cx)));
            enc.push(ui::settings_row("Recovery key", copy.into_any_element(), cx));
        }
        let encryption = ui::settings_group(
            "g-encrypt",
            Some("Encryption"),
            Some("Files are encrypted on this Mac before upload. Finder mount is off while this is on."),
            enc,
            cx,
        );

        let mut rows: Vec<AnyElement> = self
            .cfg
            .syncs
            .iter()
            .enumerate()
            .map(|(ix, pair)| {
                div()
                    .id(("sync", ix))
                    .flex_none()
                    .h(px(44.0))
                    .px(px(14.0))
                    .flex()
                    .items_center()
                    .rounded(radius)
                    .text_sm()
                    .when(self.sync_sel == Some(ix), |row| row.bg(selection))
                    .when(encrypt, |row| row.opacity(0.5))
                    .child(div().flex_1().min_w_0().truncate().child(format!("{}  ⇄  /{}", pair.folder, pair.prefix)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sync_sel = Some(ix);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        if rows.is_empty() {
            rows.push(div().h(px(44.0)).px(px(14.0)).flex().items_center().text_sm().text_color(subtle).child("No synced folders").into_any_element());
        }
        let remove = self.sync_sel;
        rows.push(
            div()
                .h(px(28.0))
                .px_1()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    IconButton::new("sync-add", IconName::Plus)
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Sm)
                        .tooltip("Sync a folder")
                        .disabled(encrypt)
                        .on_click(cx.listener(|this, _, _, cx| this.add_sync(cx))),
                )
                .child(
                    IconButton::new("sync-remove", IconName::Minus)
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Sm)
                        .tooltip("Stop syncing")
                        .disabled(encrypt || remove.is_none())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(ix) = remove {
                                this.remove_sync(ix, cx)
                            }
                        })),
                )
                .into_any_element(),
        );
        let sync = ui::settings_group(
            "g-sync",
            Some("Sync folders"),
            Some(if encrypt { "Paused while encryption is on." } else { "Syncs a folder with the one open in the Library." }),
            rows,
            cx,
        );

        let updates = ui::settings_group(
            "g-updates",
            Some("Updates"),
            None,
            vec![ui::settings_row("Check for updates automatically", toggle("set-updates", self.cfg.check_updates, Self::set_updates), cx)],
            cx,
        );

        let disconnect = ui::settings_group(
            "g-disconnect",
            None,
            None,
            vec![
                div()
                    .id("disconnect")
                    .h(px(44.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(danger)
                    .child("Disconnect…")
                    .on_click(cx.listener(|this, _, _, cx| this.open_panel(Panel::Disconnect, cx)))
                    .into_any_element(),
            ],
            cx,
        );

        div()
            .id("settings-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .child(general)
            .child(bucket)
            .child(encryption)
            .child(sync)
            .child(updates)
            .child(disconnect)
            .child(div().text_xs().text_color(subtle).text_center().child(format!("Shelf {}", env!("CARGO_PKG_VERSION"))))
    }
}

impl Render for Library {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        // `act(f)` is a callback that runs `f` on this view.
        let act = |f: fn(&mut Self, &mut Context<Self>)| {
            let this = this.clone();
            move |cx: &mut App| this.update(cx, |this, cx| f(this, cx))
        };
        let mounted = cx.global::<Bg>().mount.is_some();
        let (muted, subtle, surface, border) = {
            let c = &cx.theme().colors;
            (c.fg_muted, c.fg_subtle, c.surface, c.border)
        };
        // (is a folder, is encrypted)
        let selected = self.current().map(|e| (e.size.is_none(), crypt::is_encrypted(&e.name)));
        let encrypt = self.cfg.encrypt;
        let query = self.query.read(cx).text().to_lowercase();
        let over = self.over && cx.has_active_drag();

        // The right-click menu, and what the hover "more" button opens.
        let mut menu = Menu::new();
        let item = |label: &'static str, icon: IconName, run: Box<dyn Fn(&mut App)>| {
            MenuItem::new(label).icon(icon).on_click(move |_, cx| run(cx))
        };
        if let Some((false, encrypted)) = selected {
            // Share links would hand out ciphertext, so encrypted files get none.
            if !encrypted {
                menu = menu.item(item("Copy share link", IconName::Link, Box::new(act(Self::share))));
            }
            menu = menu
                .item(item("Download", IconName::Download, Box::new(act(Self::download))))
                .item(item("Rename", IconName::Pencil, Box::new(act(Self::ask_rename))));
        }
        if selected.is_some() {
            menu = menu.item(item("Delete", IconName::Trash2, Box::new(act(Self::ask_delete))));
        } else {
            menu = menu.item(item("Refresh", IconName::RefreshCw, Box::new(act(Self::refresh))));
        }

        // Toolbar.
        let parts: Vec<String> = self.prefix.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
        let mut crumbs = vec![Crumb::new("", self.cfg.bucket.clone()).icon(IconName::HardDrive)];
        crumbs.extend(parts.iter().enumerate().map(|(ix, part)| Crumb::new(ix.to_string(), part.clone())));
        let crumbs = Breadcrumb::new("crumbs", crumbs).on_select({
            let this = this.clone();
            move |level, _, _, cx| {
                this.update(cx, |this, cx| {
                    let prefix = this.prefix.split('/').filter(|s| !s.is_empty()).take(level).map(|p| format!("{p}/")).collect();
                    this.go(prefix, cx);
                })
            }
        });
        let search = if window.viewport_size().width < px(640.0) && !self.search_open && query.is_empty() {
            let this = this.clone();
            tool("t-search", IconName::Search, "Search", ControlSize::Md, move |cx| {
                this.update(cx, |this, cx| {
                    this.search_open = true;
                    cx.notify();
                })
            })
            .into_any_element()
        } else {
            div().w(px(180.0)).child(SearchInput::new("search", &self.query).size(ControlSize::Sm)).into_any_element()
        };
        let toolbar = drag_region("toolbar", window, cx)
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .h(px(ui::TOOLBAR_H))
            .pl(px(78.0))
            .pr_3()
            .child(
                div()
                    .flex()
                    .gap_0p5()
                    .child(tool("t-back", IconName::ArrowLeft, "Back", ControlSize::Md, act(Self::back)).disabled(self.history.is_empty()))
                    .child(tool("t-fwd", IconName::ArrowRight, "Forward", ControlSize::Md, act(Self::forward)).disabled(self.forward.is_empty())),
            )
            .child(div().flex_1().min_w_0().overflow_hidden().child(crumbs))
            .child(
                SegmentedControl::new("view", if self.view == View::Grid { "grid" } else { "list" })
                    .segment("list", "", Some(IconName::List))
                    .segment("grid", "", Some(IconName::LayoutGrid))
                    .size(ControlSize::Sm)
                    .on_change({
                        let this = this.clone();
                        move |value, _, cx| {
                            let grid = &**value == "grid";
                            this.update(cx, |this, cx| {
                                this.view = if grid { View::Grid } else { View::List };
                                cx.notify();
                            })
                        }
                    }),
            )
            .child(search)
            .child(
                tool(
                    "t-mount",
                    IconName::HardDrive,
                    if mounted { "Eject" } else { "Show in Finder" },
                    ControlSize::Md,
                    act(Self::toggle_mount),
                )
                .disabled(encrypt || self.mounting),
            )
            .child(tool("t-settings", IconName::Settings, "Settings", ControlSize::Md, {
                let this = this.clone();
                move |cx| this.update(cx, |this, cx| this.open_panel(Panel::Settings, cx))
            }));

        // Click selects, double-click opens, right-click selects and opens the menu.
        let wire = |el: gpui::Stateful<gpui::Div>, name: SharedString, folder: bool| {
            let right = name.clone();
            el.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.selected = Some(name.clone());
                window.focus(&this.focus, cx);
                if event.click_count() >= 2 {
                    this.open_entry(&name, folder, cx);
                } else {
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.selected = Some(right.clone());
                window.focus(&this.focus, cx);
                this.open_menu(event.position, cx);
                cx.stop_propagation();
            }))
        };

        let rows: Vec<FileRowData> = self
            .entries
            .iter()
            .filter(|e| query.is_empty() || shown(e).to_lowercase().contains(&query))
            .map(row_data)
            .collect();
        let items = self.entries.len();
        let total: u64 = self.entries.iter().filter_map(|e| e.size).sum();
        let usage = format!("{items} item{} · {}", if items == 1 { "" } else { "s" }, bytes(total));

        let content: AnyElement = if let Some((short, details)) = &self.error {
            let (headline, hint) = error_copy(short);
            let toggle = div().id("error-details").on_click(cx.listener(|this, _, _, cx| {
                this.show_details = !this.show_details;
                cx.notify();
            }));
            let retry = div().id("error-retry").on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
            let card = ui::error_card(headline, hint, details.clone(), self.show_details, retry, toggle, cx);
            div().size_full().flex().items_center().justify_center().child(card).into_any_element()
        } else if self.loading && self.entries.is_empty() {
            let skeleton = (0..3usize)
                .map(|ix| {
                    div()
                        .h(px(40.0))
                        .px(px(14.0))
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .child(Skeleton::new(("sk-icon", ix)).w(px(24.0)).h(px(24.0)))
                        .child(Skeleton::new(("sk-name", ix)).w(px(180.0)).h(px(12.0)))
                        .into_any_element()
                })
                .collect();
            ui::group_card("files", skeleton, 48.0, cx)
        } else if self.entries.is_empty() {
            let headline = if over { "Release to upload" } else { "Drop files here" };
            let target = ui::drop_target("empty", headline, "or click to choose files", over, cx)
                .on_click(cx.listener(|this, _, _, cx| this.choose(cx)));
            div().size_full().pb_2().child(target).into_any_element()
        } else if rows.is_empty() {
            div().py_8().text_center().text_sm().text_color(muted).child("No matches").into_any_element()
        } else {
            let on = |name: &SharedString| self.selected.as_ref() == Some(name);
            let list = match self.view {
                View::List => {
                    let rows = rows
                        .iter()
                        .enumerate()
                        .map(|(ix, data)| {
                            let mut actions: Vec<AnyElement> = Vec::new();
                            if !data.folder && !data.encrypted {
                                let (this, name) = (this.clone(), data.name.clone());
                                actions.push(
                                    tool(("link", ix), IconName::Link, "Copy share link", ControlSize::Sm, move |cx| {
                                        this.update(cx, |this, cx| {
                                            this.selected = Some(name.clone());
                                            this.share(cx);
                                        })
                                    })
                                    .into_any_element(),
                                );
                            }
                            let name = data.name.clone();
                            actions.push(
                                IconButton::new(("more", ix), IconName::Ellipsis)
                                    .variant(ButtonVariant::Ghost)
                                    .size(ControlSize::Sm)
                                    .tooltip("More")
                                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                        this.selected = Some(name.clone());
                                        this.open_menu(event.position(), cx);
                                    }))
                                    .into_any_element(),
                            );
                            wire(ui::file_row(("row", ix), data, on(&data.name), actions, cx), data.name.clone(), data.folder).into_any_element()
                        })
                        .collect();
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_none()
                                .h(px(24.0))
                                .pl(px(HEADER_PAD.0))
                                .pr(px(HEADER_PAD.1))
                                .flex()
                                .items_center()
                                .gap(px(12.0))
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(subtle)
                                .child(div().flex_1().child("Name ↑"))
                                .child(div().w(px(80.0)).text_right().child("Size"))
                                .child(div().w(px(120.0)).child("Modified"))
                                .child(div().w(px(52.0))), // the hover actions' column
                        )
                        .child(ui::group_card("files", rows, 48.0, cx))
                        .into_any_element()
                }
                View::Grid => {
                    let tiles = rows
                        .iter()
                        .enumerate()
                        .map(|(ix, data)| wire(ui::file_tile(("tile", ix), data, on(&data.name), cx), data.name.clone(), data.folder).into_any_element())
                        .collect::<Vec<_>>();
                    let grid = div().flex().flex_wrap().gap_2().p_2().children(tiles).into_any_element();
                    ui::group_card("files", vec![grid], 0.0, cx)
                }
            };
            div().id("scroll").size_full().overflow_y_scroll().pb_2().child(list).into_any_element()
        };
        let body = div()
            .flex_1()
            .min_h_0()
            .mx_3()
            .on_mouse_down(MouseButton::Right, cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.selected = None;
                this.open_menu(event.position, cx);
            }))
            .child(ContextMenu::new("context", menu).manual(self.menu_at).size_full().child(content));

        // Transfers: only what is still moving or has failed; finished rows leave.
        let mut seen = HashSet::new();
        let shown_transfers: Vec<Transfer> = self
            .transfers
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.state != TransferState::Done && seen.insert(t.key.clone()))
            .take(6)
            .cloned()
            .collect();
        let (mut active, mut failed, mut rate, mut moved, mut all) = (0, 0, 0u64, 0u64, 0u64);
        for t in self.transfers.lock().unwrap().iter() {
            match t.state {
                TransferState::Queued => active += 1,
                TransferState::Moving { rate: r } => (active, rate) = (active + 1, rate + r),
                TransferState::Failed(_) => failed += 1,
                _ => {}
            }
            (moved, all) = (moved + t.moved, all + t.total);
        }
        let summary = if active > 0 {
            let speed = if rate > 0 { format!(" · {}/s", bytes(rate)) } else { String::new() };
            Some((format!("Uploading {active} file{}{speed}", if active == 1 { "" } else { "s" }).into(), moved as f32 / all.max(1) as f32))
        } else if failed > 0 {
            Some((format!("{failed} failed").into(), moved as f32 / all.max(1) as f32))
        } else {
            None
        };
        let strip = div()
            .id("strip")
            .flex_none()
            .when(!shown_transfers.is_empty(), |strip| {
                strip.cursor_pointer().on_click(cx.listener(|this, _, _, cx| {
                    this.show_transfers = !this.show_transfers;
                    cx.notify();
                }))
            })
            .child(ui::status_strip(summary, usage.into(), cx));
        let popover = (self.show_transfers && !shown_transfers.is_empty()).then(|| {
            div()
                .id("transfer-bar")
                .absolute()
                .left_3()
                .right_3()
                .bottom(px(ui::STRIP_H + 4.0))
                .max_h(px(200.0))
                .overflow_y_scroll()
                .py_2()
                .rounded(cx.theme().radius(Radius::Xl))
                .border_1()
                .border_color(border)
                .bg(surface)
                .shadow_lg()
                .child(TransferQueue::new("transfers", shown_transfers))
        });

        let banner = self.update.as_ref().map(|release| {
            let url = release.dmg_url.clone().unwrap_or_else(|| release.html_url.clone());
            let this = this.clone();
            div().mx_3().mb_2().rounded(px(10.0)).overflow_hidden().child(
                UpdateBanner::new("update", release.tag_name.clone(), UpdateState::Available)
                    .on_download(move |_, _| {
                        // The URL comes from the GitHub API response; only hand GitHub links to `open`.
                        if url.starts_with("https://github.com/") {
                            let _ = std::process::Command::new("open").arg(&url).spawn();
                        }
                    })
                    .on_later(move |_, cx| {
                        this.update(cx, |this, cx| {
                            this.update = None;
                            cx.notify();
                        })
                    }),
            )
        });
        let panel = self.panel.as_ref().map(|panel| {
            let close = {
                let this = this.clone();
                move |_: &mut Window, cx: &mut App| {
                    this.update(cx, |this, cx| {
                        this.panel = None;
                        cx.notify();
                    })
                }
            };
            match panel {
                Panel::Settings => Sheet::new("panel", Place::Right, "Settings", close).child(self.settings_body(cx)),
                Panel::Recovery(text) => Sheet::new("panel", Place::Right, "Save your recovery key", close.clone())
                    .child(Paragraph::new(
                        "Shown once. If you lose this key, your encrypted files are gone for good: nobody, including Shelf, can recover them.",
                    ))
                    .child(
                        div()
                            .p_3()
                            .rounded(cx.theme().radius(Radius::Md))
                            .border_1()
                            .border_color(border)
                            .font_family(cx.theme().mono_family.clone())
                            .text_xs()
                            .child(wrapped(text)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("copy-key", "Copy").primary().on_click({
                                let (this, text) = (this.clone(), text.clone());
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                    this.update(cx, |this, cx| this.toast("Copied", "Paste it somewhere safe.", Severity::Success, cx));
                                }
                            }))
                            .child(Button::new("key-saved", "I saved it").variant(ButtonVariant::Secondary).on_click(move |_, window, cx| close(window, cx))),
                    ),
                Panel::Rename(old) => Sheet::new("panel", Place::Right, "Rename", close)
                    .child(Input::new(&self.rename))
                    .child(Button::new("rename-go", "Rename").primary().on_click({
                        let (this, old) = (this.clone(), old.clone());
                        move |_, _, cx| this.update(cx, |this, cx| this.do_rename(&old, cx))
                    })),
                Panel::Delete(name) => Sheet::new("panel", Place::Right, "Delete", close.clone()).child(
                    ConfirmationCard::new("delete-confirm", format!("Delete “{}”?", crypt::strip_age(name)))
                        .body("This can't be undone. A folder is deleted with everything in it.")
                        .confirm("Delete")
                        .destructive()
                        .on_confirm({
                            let (this, name) = (this.clone(), name.clone());
                            move |_, cx| this.update(cx, |this, cx| this.do_delete(&name, cx))
                        })
                        .on_cancel(move |window, cx| close(window, cx)),
                ),
                Panel::Disconnect => Sheet::new("panel", Place::Right, "Disconnect", close.clone()).child(
                    ConfirmationCard::new("disconnect-confirm", "Disconnect Shelf?")
                        .body("Your files stay in the bucket. Shelf forgets the key on this Mac.")
                        .confirm("Disconnect")
                        .destructive()
                        .on_confirm({
                            let run = act(Self::disconnect);
                            move |_, cx| run(cx)
                        })
                        .on_cancel(move |window, cx| close(window, cx)),
                ),
            }
        });

        // Drop anywhere on the window.
        let dest = format!("Drop to upload to /{}", self.prefix.trim_end_matches('/'));
        div()
            .id("library")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Delete asks to delete, unless a text field or sheet has the keys.
                let typing = this.query.read(cx).focus_handle(cx).is_focused(window);
                if matches!(event.keystroke.key.as_str(), "backspace" | "delete") && this.panel.is_none() && !typing {
                    this.ask_delete(cx);
                }
            }))
            .on_drag_move::<ExternalPaths>(cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                let inside = event.bounds.contains(&event.event.position);
                if inside != this.over {
                    this.over = inside;
                    cx.notify();
                }
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.over = false;
                this.enqueue(paths.paths().to_vec(), cx);
            }))
            .child(toolbar)
            .children(banner)
            .child(body)
            .child(strip)
            .when(over && !self.entries.is_empty(), |root| root.child(ui::drop_overlay(dest, cx)))
            .children(popover)
            .children(panel)
            .child(ToastViewport::new("toasts", &self.toaster))
    }
}
