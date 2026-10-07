//! Start at login via a per-user LaunchAgent (no SMAppService, so it works for unsigned apps).
//! `RunAtLoad` makes launchd start it at the next login, so enabling never calls `launchctl`.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const LABEL: &str = "dev.shelf.app";

fn plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("no HOME")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

/// What launchd runs: `open -a Shelf.app` when `exe` is inside a bundle, else the binary itself.
fn program_args(exe: &Path) -> Vec<String> {
    let bundle = exe.ancestors().nth(3).filter(|a| a.extension().is_some_and(|e| e == "app"));
    match bundle {
        Some(app) => ["/usr/bin/open", "-a", &app.to_string_lossy(), "--args", "--background"].map(String::from).into(),
        None => vec![exe.to_string_lossy().into_owned(), "--background".into()],
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist(args: &[String]) -> String {
    let args: String = args.iter().map(|a| format!("    <string>{}</string>\n", escape(a))).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n  \
<key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <false/>\n  <key>ProcessType</key>\n  <string>Interactive</string>\n</dict>\n</plist>\n"
    )
}

fn desired() -> Result<String> {
    Ok(plist(&program_args(&std::env::current_exe()?)))
}

pub fn is_enabled() -> bool {
    plist_path().is_ok_and(|p| p.is_file())
}

pub fn set_enabled(on: bool) -> Result<()> {
    let path = plist_path()?;
    if on {
        std::fs::create_dir_all(path.parent().context("no parent")?)?;
        std::fs::write(&path, desired()?)?;
        return Ok(());
    }
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    // Best effort. Skipped when launchd started this very process from the raw binary: bootout would kill it.
    if std::env::var("XPC_SERVICE_NAME").is_ok_and(|n| n == LABEL) {
        return Ok(());
    }
    if let Ok(out) = std::process::Command::new("id").arg("-u").output() {
        let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let _ = std::process::Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{LABEL}")]).output();
    }
    Ok(())
}

/// If enabled, rewrites the plist when the executable moved (new app location, new npx cache dir).
pub fn refresh() {
    let (Ok(path), Ok(want)) = (plist_path(), desired()) else { return };
    if path.is_file() && std::fs::read_to_string(&path).is_ok_and(|have| have != want) {
        let _ = std::fs::write(&path, want);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_for_bundle_and_raw_exe() {
        let bundle = program_args(Path::new("/Applications/My & Shelf/Shelf.app/Contents/MacOS/shelf"));
        assert_eq!(bundle, ["/usr/bin/open", "-a", "/Applications/My & Shelf/Shelf.app", "--args", "--background"]);
        let raw = program_args(Path::new("/x/target/debug/shelf"));
        assert_eq!(raw, ["/x/target/debug/shelf", "--background"]);

        let xml = plist(&bundle);
        assert!(xml.contains("<string>/Applications/My &amp; Shelf/Shelf.app</string>"));
        assert!(xml.contains("<key>RunAtLoad</key>\n  <true/>") && xml.contains("<key>KeepAlive</key>\n  <false/>"));
        assert!(xml.contains("<string>Interactive</string>") && xml.contains("<string>dev.shelf.app</string>"));
        assert!(plist(&["a<b>".into()]).contains("<string>a&lt;b&gt;</string>"));
    }
}
