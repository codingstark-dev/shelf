# Shelf

A drag-and-drop drive for your own Cloudflare R2 / S3-compatible bucket. macOS, Rust, GPUI + Ely.
Drop files or folders on the window to upload, double-click to download, copy a 7-day share link.
No server, no account: the app talks to your bucket directly.

## Install

```sh
npx shelf-drive
```

Needs macOS 26+ (Apple Silicon or Intel) and Node 18+. The first run downloads Shelf.app from GitHub Releases,
verifies its SHA-256, caches it in `~/Library/Caches/shelf-drive/` and opens it. `--update` re-downloads,
`--uninstall` deletes the cache.

The app is unsigned for now. If macOS blocks it: System Settings, Privacy & Security, scroll down, **Open Anyway**.

Shelf starts at login; turn that off in Settings, or run `npx shelf-drive --uninstall`.

**Maintainer: cutting a release.**

1. Bump the version in `app/Cargo.toml` and `npm/package.json` (they must match; the npm launcher downloads
   the release tagged `v<npm version>`).
2. Commit, then `git tag vX.Y.Z && git push --tags`.
3. Wait for the `release` workflow: it attaches `Shelf-vX.Y.Z.dmg`, `Shelf-macos-universal.zip` and
   `Shelf-macos-universal.zip.sha256` to the GitHub Release. Optional repo secret `SHELF_CF_CLIENT_ID` and
   variable `SHELF_CF_SCOPES` enable Sign in with Cloudflare in the build.
4. `cd npm && npm publish`.

To build the bundle locally: `cargo build --release -p shelf && scripts/bundle.sh 0.1.0 target/release/shelf dist`.

## Build and run

Needs Rust 1.95+ (`rustup update`) and macOS (Liquid Glass needs macOS 26; older versions get the blurred fallback).

```sh
cargo run -p shelf          # launch
cargo test --workspace      # core tests: keys, sync, encryption, mount, updates
```

## Create an R2 API token

1. Cloudflare dashboard, R2, create a bucket.
2. R2, Manage R2 API Tokens, Create API token: permission **Object Read & Write**, scoped to that bucket.
3. Copy the **Access Key ID** and **Secret Access Key** (shown once) and your **Account ID**
   (the endpoint is `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`).
4. In Shelf's Connect card pick Cloudflare R2 and paste Account ID, bucket, key ID and secret.
   For MinIO, B2 and other S3-compatible services pick "S3-compatible" and paste the endpoint URL.

The secret is stored only in the macOS Keychain (service `shelf`). Everything else is in
`~/Library/Application Support/Shelf/config.json`. Disconnect (Settings) removes both.

## Sign in with Cloudflare

The Connect card can sign in with Cloudflare, create a private R2 bucket (`shelf-xxxxxx`) and an S3 key,
and fill in the connection for you. The button only appears in builds that have an OAuth client ID.

**Maintainer setup (once).** Cloudflare dashboard, Manage Account, OAuth clients, Create:

- grant type `authorization_code`, response type `code`, token endpoint auth method `none` (no secret, PKCE S256)
- redirect URI `http://127.0.0.1:47821/callback`
- scopes: account read, R2 storage write, account API tokens write. Get their dot-delimited IDs from
  `curl -H "Authorization: Bearer $TOKEN" https://api.cloudflare.com/client/v4/oauth/scopes`.

Build with both: `SHELF_CF_CLIENT_ID=<client id> SHELF_CF_SCOPES="<id> <id> <id>" cargo build --release`.
The default scopes in `core/src/cloudflare.rs` (`scopes()`) are guesses and will fail until replaced.

**Visibility.** A private client works for your own account straight away. Making it public (so anyone can sign in)
is permanent and needs a logo, a client URL and domain verification.

**What Shelf gets.** After sign-in it creates the bucket and an account API token whose only permission is
"Workers R2 Storage Bucket Item Write" on that one bucket. The S3 key ID is the token id and the secret is the
SHA-256 hex of the token value; the secret goes to the Keychain like any other. The OAuth token is revoked
afterwards and never stored. If R2 isn't enabled on the account, Shelf says so and links to
`https://dash.cloudflare.com/?to=/:account/r2/overview`.

## Finder, sync, encryption, updates

**Show in Finder (mount).** The tray item and the toolbar drive button mount the bucket at
`~/Shelf/<bucket>` with [rclone](https://rclone.org) (`brew install rclone`) and open it in Finder;
press again to Eject. Credentials go to rclone through environment variables, never a config file.
Off while encryption is on (Finder would show ciphertext).

**Sync folders.** Settings, "Sync a folder...": pick a local folder and it syncs two ways with the
bucket folder you have open. Both sides win safely: a conflict keeps both files, the remote one as
`name (conflict YYYY-MM-DD).ext` (then `... 2).ext`, and so on). Deleted files go to the Trash. Pairs run
while the app is in the tray. Paused while encryption is on.

**Encryption.** Settings, "Encrypt new uploads": new uploads are encrypted on your Mac with
[age](https://age-encryption.org) and stored as `name.age`; the listing shows the plain name with a lock,
and downloads decrypt. Turning it on shows a **recovery key once. Save it: if you lose it, your encrypted
files are gone for good.** It also lives in the Keychain (`age-identity:<bucket>`), is kept when you
disconnect, and is needed to read the files on another Mac (not importable in the UI yet). Share links are
hidden for encrypted files.

**Updates.** At most once a day Shelf asks GitHub for the latest release and shows a banner with
Download. Turn it off in Settings, "Check for updates". The repo it asks is `REPO` in `app/src/library.rs` (`codingstark-dev/shelf`).
