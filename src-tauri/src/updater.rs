use crate::commands::http_client;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

const PRODUCTION_RELEASES_API: &str = "https://api.github.com/repos/kyizl/kyra/releases/latest";
const DEV_RELEASES_API: &str = "https://api.github.com/repos/kyizl/kyra/releases/tags/dev-latest";
const STATUS_EVENT: &str = "updater:status";
const USER_AGENT: &str = "Kyra-Overlay";
const AUTO_INSTALL_DELAY: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const SHORT_COMMIT_LENGTH: usize = 7;

#[derive(Default)]
pub struct UpdaterState {
    busy: AtomicBool,
    staged: Mutex<Option<PathBuf>>,
}

impl UpdaterState {
    fn stage(&self, installer: PathBuf) {
        if let Ok(mut staged) = self.staged.lock() {
            *staged = Some(installer);
        }
    }

    fn take_staged(&self) -> Option<PathBuf> {
        self.staged.lock().ok().and_then(|mut staged| staged.take())
    }
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: Option<u64>,
}

struct ReleaseInfo {
    label: String,
    available: bool,
    asset: Option<ReleaseAsset>,
}

fn is_dev_channel() -> bool {
    option_env!("KYRA_UPDATE_CHANNEL") == Some("dev")
}

fn build_commit() -> Option<&'static str> {
    option_env!("KYRA_BUILD_COMMIT").filter(|commit| !commit.is_empty())
}

fn releases_api() -> &'static str {
    if is_dev_channel() {
        DEV_RELEASES_API
    } else {
        PRODUCTION_RELEASES_API
    }
}

fn installer_suffix() -> &'static str {
    if cfg!(target_os = "windows") {
        ".exe"
    } else if cfg!(target_os = "macos") {
        ".dmg"
    } else {
        ".appimage"
    }
}

fn release_version(tag: Option<&str>) -> &str {
    tag.unwrap_or_default().trim_start_matches('v')
}

fn parse_version(version: &str) -> Option<[u64; 3]> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let parsed = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(parsed)
}

fn is_newer(release: &str, current: &str) -> bool {
    matches!(
        (parse_version(release), parse_version(current)),
        (Some(release), Some(current)) if release > current
    )
}

fn short_commit(commit: &str) -> &str {
    &commit[..commit.len().min(SHORT_COMMIT_LENGTH)]
}

fn dev_release_available(body: &str, commit: Option<&str>) -> bool {
    commit.is_some_and(|commit| !body.contains(commit))
}

fn dev_release_label(release: &Value) -> String {
    let commit = release
        .get("body")
        .and_then(Value::as_str)
        .and_then(|body| body.split("commit ").nth(1))
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_hexdigit)
                .collect::<String>()
        })
        .filter(|commit| !commit.is_empty());
    match commit {
        Some(commit) => format!("dev-{}", short_commit(&commit)),
        None => "dev-latest".to_owned(),
    }
}

fn find_installer(release: &Value) -> Option<ReleaseAsset> {
    release
        .get("assets")?
        .as_array()?
        .iter()
        .filter_map(|asset| serde_json::from_value::<ReleaseAsset>(asset.clone()).ok())
        .find(|asset| {
            asset
                .name
                .to_ascii_lowercase()
                .ends_with(installer_suffix())
        })
}

fn evaluate_release(release: &Value) -> ReleaseInfo {
    let asset = find_installer(release);
    if is_dev_channel() {
        let body = release
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return ReleaseInfo {
            label: dev_release_label(release),
            available: dev_release_available(body, build_commit()),
            asset,
        };
    }
    let version = release_version(release.get("tag_name").and_then(Value::as_str));
    ReleaseInfo {
        label: version.to_owned(),
        available: is_newer(version, env!("CARGO_PKG_VERSION")),
        asset,
    }
}

fn current_label() -> String {
    match build_commit() {
        Some(commit) if is_dev_channel() => format!("dev-{}", short_commit(commit)),
        _ => env!("CARGO_PKG_VERSION").to_owned(),
    }
}

fn emit_status(app: &AppHandle, payload: Value) {
    if let Err(error) = app.emit(STATUS_EVENT, payload) {
        eprintln!("updater status event failed: {error}");
    }
}

fn report(app: &AppHandle, outcome: Result<(), String>) {
    if let Err(error) = outcome {
        emit_status(app, json!({ "status": "error", "error": error }));
    }
}

async fn fetch_release() -> Result<Value, String> {
    http_client()?
        .get(releases_api())
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())
}

fn verify_digest(asset: &ReleaseAsset, bytes: &[u8]) -> Result<(), String> {
    let Some(expected) = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
    else {
        return Ok(());
    };
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err("downloaded installer checksum does not match the release digest".to_owned())
    }
}

async fn download(app: &AppHandle, asset: &ReleaseAsset, label: &str) -> Result<PathBuf, String> {
    emit_status(
        app,
        json!({ "status": "downloading", "version": label, "percent": 0 }),
    );
    let mut response = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?
        .get(&asset.browser_download_url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    let total = response
        .content_length()
        .or(asset.size)
        .filter(|total| *total > 0);
    let mut bytes = Vec::new();
    let mut reported = 0_u64;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        bytes.extend_from_slice(&chunk);
        let Some(total) = total else {
            continue;
        };
        let percent = ((bytes.len() as u64).saturating_mul(100) / total).min(100);
        if percent != reported {
            reported = percent;
            emit_status(
                app,
                json!({ "status": "downloading", "version": label, "percent": percent }),
            );
        }
    }
    verify_digest(asset, &bytes)?;
    let path = std::env::temp_dir().join(&asset.name);
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|error| error.to_string())?;
    Ok(path)
}

#[cfg(target_os = "windows")]
fn launch_installer(app: &AppHandle, installer: &Path) -> Result<(), String> {
    std::process::Command::new(installer)
        .args(["/P", "/R"])
        .spawn()
        .map_err(|error| error.to_string())?;
    app.exit(0);
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch_installer(app: &AppHandle, installer: &Path) -> Result<(), String> {
    opener::open(installer).map_err(|error| error.to_string())?;
    app.exit(0);
    Ok(())
}

#[cfg(target_os = "linux")]
fn launch_installer(app: &AppHandle, installer: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let target = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .ok_or_else(|| "automatic updates require running the AppImage build".to_owned())?;
    let mut staged_name = target.clone().into_os_string();
    staged_name.push(".update");
    let staged = PathBuf::from(staged_name);
    let replaced = std::fs::copy(installer, &staged)
        .and_then(|_| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&staged, &target));
    if let Err(error) = replaced {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("could not replace {}: {error}", target.display()));
    }
    let _ = std::fs::remove_file(installer);
    std::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 1 && exec \"$0\"")
        .arg(&target)
        .spawn()
        .map_err(|error| error.to_string())?;
    app.exit(0);
    Ok(())
}

fn install_staged(app: &AppHandle, state: &UpdaterState) -> Result<(), String> {
    let Some(installer) = state.take_staged() else {
        return Ok(());
    };
    emit_status(app, json!({ "status": "installing" }));
    if let Err(error) = launch_installer(app, &installer) {
        state.stage(installer);
        return Err(error);
    }
    Ok(())
}

async fn run_check(app: &AppHandle, state: &UpdaterState) -> Result<(), String> {
    emit_status(app, json!({ "status": "checking" }));
    let release = fetch_release().await?;
    let info = evaluate_release(&release);
    if !info.available {
        emit_status(
            app,
            json!({ "status": "up-to-date", "version": current_label() }),
        );
        return Ok(());
    }
    let asset = info
        .asset
        .ok_or_else(|| "no installer asset is available for this platform".to_owned())?;
    emit_status(app, json!({ "status": "available", "version": info.label }));
    let installer = download(app, &asset, &info.label).await?;
    state.stage(installer);
    emit_status(
        app,
        json!({ "status": "downloaded", "version": info.label }),
    );
    tokio::time::sleep(AUTO_INSTALL_DELAY).await;
    install_staged(app, state)
}

#[tauri::command]
pub async fn updater_check(app: AppHandle, state: State<'_, UpdaterState>) -> Result<(), String> {
    if state.busy.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let outcome = run_check(&app, &state).await;
    state.busy.store(false, Ordering::Release);
    report(&app, outcome);
    Ok(())
}

#[tauri::command]
pub fn updater_install(app: AppHandle, state: State<'_, UpdaterState>) -> Result<(), String> {
    report(&app, install_staged(&app, &state));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_release_versions() {
        assert_eq!(release_version(Some("v1.11.1")), "1.11.1");
        assert_eq!(release_version(Some("1.12.0")), "1.12.0");
        assert_eq!(release_version(None), "");
    }

    #[test]
    fn compares_semantic_versions() {
        assert!(is_newer("1.11.2", "1.11.1"));
        assert!(is_newer("1.12.0", "1.11.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.11.1", "1.11.1"));
        assert!(!is_newer("1.10.9", "1.11.0"));
        assert!(is_newer("1.12.0-beta.1", "1.11.0"));
    }

    #[test]
    fn rejects_unparseable_versions() {
        assert!(!is_newer("", "1.11.1"));
        assert!(!is_newer("dev-latest", "1.11.1"));
        assert!(!is_newer("1.11", "1.10.0"));
        assert!(!is_newer("1.11.2.1", "1.11.1"));
    }

    #[test]
    fn detects_when_the_dev_build_is_current() {
        let body = "Automated development build from dev commit 0cf3a2b94f1d.";
        assert!(!dev_release_available(body, Some("0cf3a2b94f1d")));
        assert!(!dev_release_available(body, Some("0cf3a2b")));
        assert!(dev_release_available(body, Some("ffffffffffff")));
        assert!(!dev_release_available(body, None));
    }

    #[test]
    fn labels_dev_releases_with_a_short_commit() {
        let release = json!({
            "body": "Automated development build from dev commit 0cf3a2b94f1d5e6a7b8c9d0e1f2a3b4c5d6e7f80."
        });
        assert_eq!(dev_release_label(&release), "dev-0cf3a2b");
        assert_eq!(
            dev_release_label(&json!({ "body": "unrelated" })),
            "dev-latest"
        );
        assert_eq!(dev_release_label(&json!({})), "dev-latest");
    }

    #[test]
    fn selects_the_installer_for_this_platform() {
        let release = json!({
            "tag_name": "v999.0.0",
            "assets": [
                { "name": "Kyra.Overlay_999.0.0_x64-setup.exe", "browser_download_url": "https://example.test/a.exe" },
                { "name": "Kyra.Overlay_999.0.0_x64-setup.exe.blockmap", "browser_download_url": "https://example.test/a.map" },
                { "name": "Kyra.Overlay_999.0.0_universal.dmg", "browser_download_url": "https://example.test/a.dmg" },
                { "name": "Kyra.Overlay_999.0.0_amd64.AppImage", "browser_download_url": "https://example.test/a.AppImage" }
            ]
        });
        let asset = find_installer(&release).expect("installer for the current platform");
        assert!(asset
            .name
            .to_ascii_lowercase()
            .ends_with(installer_suffix()));
        assert!(!asset.name.ends_with(".blockmap"));
    }

    #[test]
    fn evaluates_a_newer_production_release() {
        let release = json!({ "tag_name": "v999.0.0", "assets": [] });
        let info = evaluate_release(&release);
        assert!(info.available);
        assert_eq!(info.label, "999.0.0");
        assert!(info.asset.is_none());
    }

    #[test]
    fn verifies_the_release_digest() {
        let asset = ReleaseAsset {
            name: "installer".to_owned(),
            browser_download_url: String::new(),
            digest: Some(format!("sha256:{:x}", Sha256::digest(b"payload"))),
            size: None,
        };
        assert!(verify_digest(&asset, b"payload").is_ok());
        assert!(verify_digest(&asset, b"tampered").is_err());
        let unsigned = ReleaseAsset {
            digest: None,
            ..asset
        };
        assert!(verify_digest(&unsigned, b"anything").is_ok());
    }

    #[test]
    fn shortens_commits() {
        assert_eq!(short_commit("0cf3a2b94f1d"), "0cf3a2b");
        assert_eq!(short_commit("abc"), "abc");
    }
}
