use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum TrafficMode {
    #[default]
    Middleware,
    Direct,
}

#[derive(Default, Deserialize, Serialize)]
struct TrafficConfig {
    mode: TrafficMode,
    #[serde(default, rename = "hasApiKey")]
    has_api_key: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficStatus {
    mode: TrafficMode,
    has_api_key: bool,
}

#[derive(Default)]
pub struct TrafficState(Mutex<()>);

pub struct StatsRoute {
    pub base: &'static str,
    pub headers: HeaderMap,
}

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new("kyra-overlay", "pika-stats-api-key").map_err(|_| {
        "Secure storage is unavailable. Unlock your system keyring and try again.".to_owned()
    })
}

fn api_key() -> Result<Option<String>, String> {
    match entry()?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => {
            Err("Could not read the API key. Unlock your system keyring and try again.".to_owned())
        }
    }
}

fn load(directory: &Path) -> Result<TrafficConfig, String> {
    match std::fs::read(directory.join("traffic.json")) {
        Ok(data) => serde_json::from_slice(&data).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TrafficConfig::default()),
        Err(error) => Err(error.to_string()),
    }
}

fn persist(directory: &Path, mode: TrafficMode, has_api_key: bool) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let data = serde_json::to_vec(&TrafficConfig { mode, has_api_key })
        .map_err(|error| error.to_string())?;
    let temporary = directory.join("traffic.json.tmp");
    std::fs::write(&temporary, data).map_err(|error| error.to_string())?;
    std::fs::rename(temporary, directory.join("traffic.json")).map_err(|error| error.to_string())
}

fn route(network: &str, mode: TrafficMode, key: Option<&str>) -> Result<StatsRoute, String> {
    let mut headers = HeaderMap::new();
    let base = match (network, mode) {
        ("jartex", _) => "https://stats.jartexnetwork.com/api",
        ("pika", TrafficMode::Middleware) => "https://proxy.kyizl.is-a.dev/api",
        ("pika", TrafficMode::Direct) => {
            let key = key.ok_or("Save an API key in Settings → Traffic first.")?;
            let mut value =
                HeaderValue::from_str(key).map_err(|_| "The saved API key is invalid.")?;
            value.set_sensitive(true);
            headers.insert("X-Api-Key", value);
            "https://stats.pika-network.net/api"
        }
        _ => return Err("unsupported statistics network".to_owned()),
    };
    Ok(StatsRoute { base, headers })
}

impl TrafficState {
    pub fn stats_route(&self, app: &AppHandle, network: &str) -> Result<StatsRoute, String> {
        if network == "jartex" {
            return route(network, TrafficMode::Middleware, None);
        }
        let _guard = self
            .0
            .lock()
            .map_err(|_| "Traffic settings lock poisoned.")?;
        let directory = app
            .path()
            .app_data_dir()
            .map_err(|error| error.to_string())?;
        let config = load(&directory)?;
        let key = if config.mode == TrafficMode::Direct {
            api_key()?
        } else {
            None
        };
        route(network, config.mode, key.as_deref())
    }
}

#[tauri::command]
pub fn traffic_get(
    app: AppHandle,
    state: State<'_, TrafficState>,
) -> Result<TrafficStatus, String> {
    let _guard = state
        .inner()
        .0
        .lock()
        .map_err(|_| "Traffic settings lock poisoned.")?;
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    let config = load(&directory)?;
    Ok(TrafficStatus {
        mode: config.mode,
        has_api_key: config.has_api_key,
    })
}

#[tauri::command]
pub fn traffic_set_mode(
    app: AppHandle,
    state: State<'_, TrafficState>,
    mode: TrafficMode,
) -> Result<TrafficStatus, String> {
    let _guard = state
        .inner()
        .0
        .lock()
        .map_err(|_| "Traffic settings lock poisoned.")?;
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    let has_api_key = load(&directory)?.has_api_key;
    if mode == TrafficMode::Direct && api_key()?.is_none() {
        return Err("Save an API key in Settings → Traffic first.".to_owned());
    }
    persist(&directory, mode, has_api_key)?;
    Ok(TrafficStatus { mode, has_api_key })
}

#[tauri::command]
pub fn traffic_save_api_key(
    app: AppHandle,
    state: State<'_, TrafficState>,
    api_key: String,
) -> Result<TrafficStatus, String> {
    let key = api_key.trim();
    if key.is_empty() || !key.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err("Enter a valid API key without spaces or control characters.".to_owned());
    }
    let _guard = state
        .inner()
        .0
        .lock()
        .map_err(|_| "Traffic settings lock poisoned.")?;
    entry()?
        .set_password(key)
        .map_err(|_| "Could not save the API key. Unlock your system keyring and try again.")?;
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    persist(&directory, TrafficMode::Direct, true)?;
    Ok(TrafficStatus {
        mode: TrafficMode::Direct,
        has_api_key: true,
    })
}

#[tauri::command]
pub fn traffic_remove_api_key(
    app: AppHandle,
    state: State<'_, TrafficState>,
) -> Result<TrafficStatus, String> {
    let _guard = state
        .inner()
        .0
        .lock()
        .map_err(|_| "Traffic settings lock poisoned.")?;
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(_) => {
            return Err(
                "Could not remove the API key. Unlock your system keyring and try again."
                    .to_owned(),
            )
        }
    }
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    persist(&directory, TrafficMode::Middleware, false)?;
    Ok(TrafficStatus {
        mode: TrafficMode::Middleware,
        has_api_key: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_middleware_and_persists_mode_without_secrets() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            load(directory.path()).unwrap().mode,
            TrafficMode::Middleware
        );
        persist(directory.path(), TrafficMode::Direct, true).unwrap();
        assert_eq!(load(directory.path()).unwrap().mode, TrafficMode::Direct);
        let data = std::fs::read_to_string(directory.path().join("traffic.json")).unwrap();
        assert_eq!(data, r#"{"mode":"direct","hasApiKey":true}"#);
        persist(directory.path(), TrafficMode::Middleware, false).unwrap();
        assert_eq!(
            load(directory.path()).unwrap().mode,
            TrafficMode::Middleware
        );
    }

    #[test]
    fn only_direct_pika_requests_receive_the_api_key() {
        let direct = route("pika", TrafficMode::Direct, Some("test-key")).unwrap();
        assert_eq!(direct.base, "https://stats.pika-network.net/api");
        assert_eq!(direct.headers["X-Api-Key"], "test-key");
        assert!(direct.headers["X-Api-Key"].is_sensitive());
        let middleware = route("pika", TrafficMode::Middleware, Some("test-key")).unwrap();
        assert_eq!(middleware.base, "https://proxy.kyizl.is-a.dev/api");
        assert!(middleware.headers.is_empty());
        let jartex = route("jartex", TrafficMode::Direct, Some("test-key")).unwrap();
        assert_eq!(jartex.base, "https://stats.jartexnetwork.com/api");
        assert!(jartex.headers.is_empty());
        assert!(route("pika", TrafficMode::Direct, None).is_err());
        assert!(route("pika", TrafficMode::Direct, Some("bad\r\nkey")).is_err());
        assert!(route("unknown", TrafficMode::Middleware, None).is_err());
    }
}
