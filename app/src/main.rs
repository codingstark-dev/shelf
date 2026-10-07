mod connect;
mod library;
mod ui;

use ely_gpui_component::{
    Assets,
    primitives::FocusScope,
    shell::{TrayIcon, TrayItem, drag_region},
    theme::{ActiveTheme, Theme},
};
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Global, IntoElement, KeyBinding, ParentElement, Render, SharedString, Styled, Subscription,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowOptions, actions, div, px, size,
};
use shelf_core::{BucketConfig, Store, SyncPair, login, mount::Mount, sync::WatchHandle};

use connect::Connect;
use library::Library;
use ui::TrayState;

/// Tokio runtime for object_store; GPUI tasks await the JoinHandles.
pub struct Rt(pub tokio::runtime::Runtime);
impl Global for Rt {}

/// Keeps the tray icon alive (dropping it removes it), with what it was built from.
struct TrayHold(#[allow(dead_code)] TrayIcon, (bool, TrayState, Vec<SharedString>));
impl Global for TrayHold {}

/// What outlives the window: the Finder mount and the folder syncs.
#[derive(Default)]
pub struct Bg {
    pub mount: Option<Mount>,
    pub syncs: Vec<(SyncPair, WatchHandle)>,
    syncs_started: bool,
    /// A launch-time mount is in flight (see `mount_at_login`).
    pub mounting: bool,
    /// The open library's tray state and recent uploads (see `sync_tray`).
    tray: Option<(TrayState, Vec<SharedString>)>,
}
impl Global for Bg {}

/// Starts watching one folder pair on the tokio runtime.
pub fn start_sync(pair: &SyncPair, store: &Store, cx: &mut App) -> anyhow::Result<()> {
    let rt = cx.global::<Rt>().0.handle().clone();
    let _guard = rt.enter(); // `watch` spawns onto the current runtime
    let handle = shelf_core::sync::watch_pair(store.clone(), pair)?;
    cx.global_mut::<Bg>().syncs.push((pair.clone(), handle));
    Ok(())
}

/// Starts every saved pair, unless encryption is on (sync would upload plaintext).
pub fn start_syncs(cfg: &BucketConfig, secret: &str, cx: &mut App) {
    if cfg.encrypt || cfg.syncs.is_empty() {
        return;
    }
    let Ok(store) = shelf_core::open(cfg, secret) else { return };
    for pair in &cfg.syncs {
        if let Err(error) = start_sync(pair, &store, cx) {
            log::error!("sync {}: {error:#}", pair.folder);
        }
    }
}

/// Launch-time `start_syncs`: a second window or reconnect must not start them twice.
pub fn start_syncs_once(cfg: &BucketConfig, secret: &str, cx: &mut App) {
    if !std::mem::replace(&mut cx.global_mut::<Bg>().syncs_started, true) {
        start_syncs(cfg, secret, cx);
    }
}

#[derive(Clone, Copy)]
enum TrayCmd {
    Recent,
    Mount,
    Open,
    Quit,
    None,
}

/// A label, or `None` for a separator.
fn tray_icon(symbol: &str, labels: &[Option<String>], cmds: Vec<TrayCmd>, cx: &mut App) -> anyhow::Result<TrayIcon> {
    let items = labels
        .iter()
        .map(|label| match label {
            Some(label) => TrayItem::Action(label.clone().into()),
            None => TrayItem::Separator,
        })
        .collect();
    TrayIcon::new(
        symbol,
        items,
        move |index, cx| match cmds[index] {
            TrayCmd::Recent | TrayCmd::Open => open(cx),
            TrayCmd::Mount => {
                open(cx);
                if let Some(window) = cx.windows().first().and_then(|w| w.downcast::<Shelf>()) {
                    window
                        .update(cx, |shelf, _, cx| {
                            if let Screen::Library(library) = &shelf.screen {
                                library.update(cx, |library, cx| library.toggle_mount(cx));
                            }
                        })
                        .ok();
                }
            }
            TrayCmd::Quit => quit(cx),
            TrayCmd::None => {}
        },
        cx,
    )
}

/// The tray menu; rebuilt only when its mount state, symbol or recent list changes, because its labels can't change.
/// Reads only globals (`Bg.tray` is kept fresh by `sync_tray` and `Library::sync_tray`): it is called from
/// inside Shelf and Library updates, where reading either entity would panic with a double lease.
pub fn tray(cx: &mut App) {
    let bg = cx.global::<Bg>();
    let (state, recent) = bg.tray.clone().unwrap_or((TrayState::Idle, Vec::new()));
    let key = (bg.mount.is_some(), state, recent);
    if cx.try_global::<TrayHold>().is_some_and(|hold| hold.1 == key) {
        return;
    }
    let mut menu: Vec<(Option<String>, TrayCmd)> = key
        .2
        .iter()
        .take(5)
        .map(|name| {
            let label = match name.chars().count() > 36 {
                true => format!("{}\u{2026}", name.chars().take(36).collect::<String>()),
                false => name.to_string(),
            };
            (Some(label), TrayCmd::Recent)
        })
        .collect();
    if menu.is_empty() {
        menu.push((Some("(No recent uploads)".into()), TrayCmd::Open));
    }
    menu.extend([
        (None, TrayCmd::None),
        (Some(if key.0 { "Eject" } else { "Show in Finder" }.into()), TrayCmd::Mount),
        (Some("Open Shelf".into()), TrayCmd::Open),
        (None, TrayCmd::None),
        (Some("Quit Shelf".into()), TrayCmd::Quit),
    ]);
    let (labels, cmds): (Vec<_>, Vec<_>) = menu.into_iter().unzip();
    // A wrong symbol name fails the build, so fall back to the plain drive.
    let icon = tray_icon(ui::tray_symbol(key.1), &labels, cmds.clone(), cx).or_else(|_| tray_icon("externaldrive", &labels, cmds, cx));
    match icon {
        Ok(icon) => {
            if cx.has_global::<TrayHold>() {
                cx.remove_global::<TrayHold>();
            }
            cx.set_global(TrayHold(icon, key));
        }
        Err(error) => log::error!("tray: {error:#}"),
    }
}

/// Pushes the library's tray state into the menu (or resets it when there is no library).
fn sync_tray(library: Option<&Entity<Library>>, cx: &mut App) {
    cx.global_mut::<Bg>().tray = library.map(|l| (l.read(cx).tray_state(), l.read(cx).recent()));
    tray(cx);
}

/// Unmounts first: dropping the `Mount` stops rclone.
fn quit(cx: &mut App) {
    drop(cx.global_mut::<Bg>().mount.take());
    cx.quit();
}

actions!(shelf, [Quit]);

enum Screen {
    Connect(Entity<Connect>),
    Library(Entity<Library>),
}

struct Shelf {
    focus: FocusHandle,
    screen: Screen,
    _sub: Subscription,
}

impl Shelf {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = shelf_core::load_config()
            .and_then(|cfg| Some((shelf_core::load_secret(&cfg.bucket).ok()?, cfg)));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let (screen, _sub) = match saved.and_then(|(secret, cfg)| Library::new(cfg, &secret, window, cx).ok()) {
            Some(library) => Self::library(library, window, cx),
            None => Self::connect(window, cx),
        };
        Self { focus, screen, _sub }
    }

    fn connect(window: &mut Window, cx: &mut Context<Self>) -> (Screen, Subscription) {
        shape(window, true);
        sync_tray(None, cx);
        let connect = cx.new(|cx| Connect::new(window, cx));
        let sub = cx.subscribe_in(&connect, window, |this, _, done: &connect::Connected, window, cx| {
            if done.0.open_at_login {
                if let Err(error) = login::set_enabled(true) {
                    log::error!("start at login: {error:#}");
                }
            }
            match Library::new(done.0.clone(), &done.1, window, cx) {
                Ok(library) => (this.screen, this._sub) = Self::library(library, window, cx),
                Err(error) => log::error!("open bucket: {error:#}"),
            }
            cx.notify();
        });
        (Screen::Connect(connect), sub)
    }

    fn library(library: Entity<Library>, window: &mut Window, cx: &mut Context<Self>) -> (Screen, Subscription) {
        shape(window, false);
        cx.observe(&library, |_, library, cx| sync_tray(Some(&library), cx)).detach();
        sync_tray(Some(&library), cx);
        let sub = cx.subscribe_in(&library, window, |this, _, _: &library::Disconnected, window, cx| {
            (this.screen, this._sub) = Self::connect(window, cx);
            cx.notify();
        });
        (Screen::Library(library), sub)
    }
}

impl Render for Shelf {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (height, fg) = {
            let theme = cx.theme();
            (theme.titlebar_height(), theme.colors.fg)
        };
        // Nothing opaque here: the glass behind the window is the background. The library draws its own toolbar.
        let title = matches!(self.screen, Screen::Connect(_)).then(|| drag_region("titlebar", window, cx).flex_none().h(height));
        let body = match &self.screen {
            Screen::Connect(view) => div().size_full().child(view.clone()),
            Screen::Library(view) => div().size_full().child(view.clone()),
        };
        FocusScope::new(&self.focus)
            .root()
            .size_full()
            .flex()
            .flex_col()
            .text_color(fg)
            .children(title)
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// Liquid Glass (macOS 26+): wraps GPUI's NSView in an NSGlassEffectView and makes that the
/// window's content view. Returns false when the class is missing (older macOS) or anything
/// else is off, and the caller falls back to `Blurred`.
#[cfg(target_os = "macos")]
fn liquid_glass(window: &Window) -> bool {
    use objc2::{
        msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject},
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(class) = AnyClass::get(c"NSGlassEffectView") else {
        return false;
    };
    let Ok(RawWindowHandle::AppKit(handle)) = HasWindowHandle::window_handle(window).map(|h| h.as_raw()) else {
        return false;
    };
    // SAFETY: `ns_view` is GPUI's live NSView; all messages below are plain AppKit calls
    // made on the main thread (window creation runs there).
    unsafe {
        let view = handle.ns_view.as_ptr().cast::<AnyObject>();
        let ns_window: *mut AnyObject = msg_send![view, window];
        if ns_window.is_null() {
            return false;
        }
        let glass: Retained<AnyObject> = msg_send![class, new];
        let _: () = msg_send![&*glass, setCornerRadius: 16.0_f64];
        let _: () = msg_send![&*glass, setContentView: view];
        let _: () = msg_send![ns_window, setContentView: &*glass];
    }
    true
}

#[cfg(not(target_os = "macos"))]
fn liquid_glass(_: &Window) -> bool {
    false
}

/// The window's shape for the Setup screen (480 x 500, fixed) or the Library (760 x 520, min 600 x 400).
fn shape(window: &mut Window, setup: bool) {
    let (width, height) = if setup { ui::SETUP_SIZE } else { ui::LIBRARY_SIZE };
    window.resize(size(px(width), px(height)));
    #[cfg(target_os = "macos")]
    {
        use objc2::{Encode, Encoding, msg_send, runtime::AnyObject};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        #[repr(C)]
        struct NSSize {
            width: f64,
            height: f64,
        }
        // SAFETY: NSSize is two CGFloats, as AppKit declares it.
        unsafe impl Encode for NSSize {
            const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
        }

        let Ok(RawWindowHandle::AppKit(handle)) = HasWindowHandle::window_handle(window).map(|h| h.as_raw()) else {
            return;
        };
        let (min_width, min_height) = if setup { ui::SETUP_SIZE } else { ui::LIBRARY_MIN };
        // SAFETY: plain AppKit calls on the main thread, on GPUI's live NSView and its window.
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<AnyObject>();
            let ns_window: *mut AnyObject = msg_send![view, window];
            if ns_window.is_null() {
                return;
            }
            let mask: usize = msg_send![ns_window, styleMask];
            let resizable = 1usize << 3; // NSWindowStyleMaskResizable
            let mask = if setup { mask & !resizable } else { mask | resizable };
            let _: () = msg_send![ns_window, setStyleMask: mask];
            let _: () = msg_send![ns_window, setContentMinSize: NSSize { width: f64::from(min_width), height: f64::from(min_height) }];
        }
    }
}

/// `cargo run` has no bundle, so give the Dock the app icon.
#[cfg(target_os = "macos")]
fn app_icon() {
    use objc2::{
        msg_send,
        runtime::{AnyClass, AnyObject},
    };
    let path = std::ffi::CString::new(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/AppIcon.icns")).expect("no NUL in path");
    let (Some(string), Some(image), Some(app)) = (AnyClass::get(c"NSString"), AnyClass::get(c"NSImage"), AnyClass::get(c"NSApplication")) else {
        return;
    };
    // SAFETY: plain AppKit calls on the main thread; a missing file gives a nil image, which AppKit ignores.
    unsafe {
        let path: *mut AnyObject = msg_send![string, stringWithUTF8String: path.as_ptr()];
        let alloc: *mut AnyObject = msg_send![image, alloc];
        let image: *mut AnyObject = msg_send![alloc, initWithContentsOfFile: path];
        if !image.is_null() {
            let app: *mut AnyObject = msg_send![app, sharedApplication];
            let _: () = msg_send![app, setApplicationIconImage: image];
        }
    }
}

fn open(cx: &mut App) {
    if let Some(window) = cx.windows().first() {
        window.update(cx, |_, window, _| window.activate_window()).ok();
        cx.activate(true);
        return;
    }
    // Guess the first screen so the window opens at the right size; `Shelf` corrects it if opening the bucket fails.
    let saved = shelf_core::load_config().is_some_and(|cfg| shelf_core::load_secret(&cfg.bucket).is_ok());
    let (width, height) = if saved { ui::LIBRARY_SIZE } else { ui::SETUP_SIZE };
    let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("Shelf".into()),
            appears_transparent: true,
            traffic_light_position: Some(cx.theme().traffic_light_origin()),
        }),
        window_background: WindowBackgroundAppearance::Transparent,
        is_resizable: saved,
        window_min_size: Some(if saved { size(px(ui::LIBRARY_MIN.0), px(ui::LIBRARY_MIN.1)) } else { size(px(width), px(height)) }),
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        let glass = liquid_glass(window);
        if !glass {
            window.set_background_appearance(WindowBackgroundAppearance::Blurred);
        }
        // Follows the system light/dark setting (and runs once now).
        window
            .observe_window_appearance(|window, cx| Theme::set_mode_now(window.appearance().into(), cx))
            .detach();
        cx.new(|cx| Shelf::new(window, cx))
    })
    .expect("window failed to open");
    cx.activate(true);
}

/// `~/Shelf/<bucket>`, where the Finder drive is mounted.
pub fn mountpoint(bucket: &str) -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join("Shelf").join(bucket))
}

/// `--background` launch: no window. Tray, folder syncs, and the Finder drive if it was mounted last time.
/// Failures are only logged; there is no window to toast in.
fn background(cx: &mut App) {
    let Some(cfg) = shelf_core::load_config() else { return };
    let Ok(secret) = shelf_core::load_secret(&cfg.bucket) else { return };
    start_syncs_once(&cfg, &secret, cx);
    if !cfg.mount_at_login || cfg.encrypt {
        return;
    }
    if shelf_core::mount::find_rclone().is_none() {
        return log::warn!("mount at login: rclone is not installed");
    }
    let Some(dir) = mountpoint(&cfg.bucket) else { return };
    cx.global_mut::<Bg>().mounting = true;
    let job = cx.global::<Rt>().0.spawn_blocking(move || {
        shelf_core::mount::mount(&cfg.endpoint, &cfg.bucket, &cfg.access_key_id, &secret, &dir)
    });
    cx.spawn(async move |cx| {
        let result = job.await.map_err(anyhow::Error::from).and_then(|r| r);
        cx.update(|cx| {
            cx.global_mut::<Bg>().mounting = false;
            match result {
                Ok(mount) => cx.global_mut::<Bg>().mount = Some(mount),
                Err(error) => log::error!("mount at login: {error:#}"),
            }
            tray(cx);
        });
    })
    .detach();
}

fn main() {
    login::refresh();
    // Users who connected before login items existed never got one; toggling it off saves `false`, so this respects that.
    if shelf_core::load_config().is_some_and(|cfg| cfg.open_at_login) && !login::is_enabled() {
        if let Err(error) = login::set_enabled(true) {
            log::error!("login item: {error:#}");
        }
    }
    let background_launch = std::env::args().any(|a| a == "--background");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    gpui_platform::application().with_assets(Assets).run(move |cx: &mut App| {
        ely_gpui_component::init(cx).expect("Ely failed to start");
        ui::install(cx);
        #[cfg(target_os = "macos")]
        app_icon();
        cx.set_global(Rt(rt));
        cx.set_global(Bg::default());
        cx.on_action(|_: &Quit, cx| quit(cx));
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        tray(cx);
        if background_launch {
            background(cx);
        } else {
            open(cx);
        }
    });
}
