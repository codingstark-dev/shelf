use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

/// Looks on PATH, then the Homebrew locations (GUI apps often lack them on PATH).
pub fn find_rclone() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .map(|d| d.join("rclone"))
        .find(|p| p.is_file())
}

/// Secrets go only through env vars, never args or an rclone.conf.
fn command(rclone: &Path, endpoint: &str, bucket: &str, key_id: &str, secret: &str, mountpoint: &Path) -> Command {
    let provider = if endpoint.contains("r2.cloudflarestorage.com") { "Cloudflare" } else { "Other" };
    let mut c = Command::new(rclone);
    c.arg("nfsmount")
        .arg(format!("shelf:{bucket}"))
        .arg(mountpoint)
        .args(["--vfs-cache-mode", "full", "--volname", "Shelf"])
        // rclone's NFS server has no lock manager: without locallocks, Finder's locked writes fail with
        // ENOLCK and copies stall at "Zero bytes".
        .args(["-o", "locallocks"])
        .env("RCLONE_CONFIG_SHELF_TYPE", "s3")
        .env("RCLONE_CONFIG_SHELF_PROVIDER", provider)
        .env("RCLONE_CONFIG_SHELF_ENDPOINT", endpoint)
        .env("RCLONE_CONFIG_SHELF_ACCESS_KEY_ID", key_id)
        .env("RCLONE_CONFIG_SHELF_SECRET_ACCESS_KEY", secret)
        .env("RCLONE_CONFIG_SHELF_ACL", "private");
    c
}

/// A live rclone mount. Dropping it unmounts and kills rclone.
pub struct Mount {
    child: Child,
    mountpoint: PathBuf,
}

impl Mount {
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

/// Blocks about 1.5 s: if rclone has exited by then (bad credentials, stale mount, ...) its
/// stderr is the error. Run it off the UI thread. ponytail: an rclone that dies later is not
/// reported; poll `try_wait` if that matters.
pub fn mount(endpoint: &str, bucket: &str, access_key_id: &str, secret: &str, mountpoint: &Path) -> Result<Mount> {
    let Some(rclone) = find_rclone() else { bail!("rclone not found; run `brew install rclone`") };
    spawn(&rclone, endpoint, bucket, access_key_id, secret, mountpoint)
}

fn spawn(rclone: &Path, endpoint: &str, bucket: &str, access_key_id: &str, secret: &str, mountpoint: &Path) -> Result<Mount> {
    std::fs::create_dir_all(mountpoint)?;
    let mut child = command(rclone, endpoint, bucket, access_key_id, secret, mountpoint)
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning rclone")?;
    let mut stderr = child.stderr.take().context("rclone stderr")?;
    std::thread::sleep(Duration::from_millis(1500));
    if let Some(status) = child.try_wait()? {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        bail!("rclone exited ({status}): {}", text.trim());
    }
    // Keep draining so a chatty rclone never blocks on a full pipe.
    std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
    Ok(Mount { child, mountpoint: mountpoint.to_owned() })
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mountpoint).status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_only_in_env() {
        let c = command(Path::new("rclone"), "https://x.r2.cloudflarestorage.com", "b", "AKID", "s3cr3t", Path::new("/tmp/m"));
        let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..3], ["nfsmount", "shelf:b", "/tmp/m"]);
        assert!(args.iter().all(|a| !a.contains("s3cr3t") && !a.contains("AKID")));
        let env = |k: &str| c.get_envs().find(|(n, _)| *n == k).and_then(|(_, v)| v).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(env("RCLONE_CONFIG_SHELF_SECRET_ACCESS_KEY").as_deref(), Some("s3cr3t"));
        assert_eq!(env("RCLONE_CONFIG_SHELF_PROVIDER").as_deref(), Some("Cloudflare"));
    }

    #[test]
    fn early_exit_reports_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |body: &str| {
            let p = dir.path().join("rclone");
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let mp = dir.path().join("mnt");
        let fail = script("echo 'bad credentials' >&2; exit 3");
        let error = spawn(&fail, "https://e", "b", "k", "s", &mp).err().unwrap().to_string();
        assert!(error.contains("bad credentials"), "{error}");
        let ok = script("exec sleep 30");
        let mount = spawn(&ok, "https://e", "b", "k", "s", &mp).unwrap();
        assert_eq!(mount.mountpoint(), mp);
    }
}
