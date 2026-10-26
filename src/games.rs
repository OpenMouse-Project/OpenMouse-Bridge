use std::{borrow::Cow, collections::BTreeSet, env, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::GameConfig;

pub const GAMES_CDN_URL: &str =
    "https://cdn.jsdelivr.net/gh/OpenMouse-Project/Desktop@main/public/games.json";
const GAMES_URL_ENV: &str = "OPENMOUSE_BRIDGE_GAMES_URL";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct CatalogResponse {
    games: Vec<GameConfig>,
}

pub fn catalog_url() -> Cow<'static, str> {
    env::var(GAMES_URL_ENV)
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(GAMES_CDN_URL))
}

pub async fn fetch_catalog() -> Result<Vec<GameConfig>> {
    let url = catalog_url();
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .context("could not create the game catalog client")?;
    let bytes = client
        .get(url.as_ref())
        .send()
        .await
        .with_context(|| format!("could not download {url}"))?
        .error_for_status()
        .with_context(|| format!("game catalog request failed for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("could not read game catalog from {url}"))?;
    parse_catalog(&bytes)
}

fn parse_catalog(bytes: &[u8]) -> Result<Vec<GameConfig>> {
    let catalog: CatalogResponse =
        serde_json::from_slice(bytes).context("could not parse the game catalog")?;
    ensure!(!catalog.games.is_empty(), "game catalog contains no games");
    Ok(catalog.games)
}

pub struct GameDetector {
    system: System,
}

impl Default for GameDetector {
    fn default() -> Self {
        Self {
            system: System::new(),
        }
    }
}

impl GameDetector {
    pub fn detect(&mut self, games: &[GameConfig]) -> Vec<String> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
        );
        let running: BTreeSet<String> = self
            .system
            .processes()
            .values()
            .filter_map(|process| {
                process
                    .exe()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().to_ascii_lowercase())
            })
            .collect();
        games
            .iter()
            .filter(|game| {
                game.executables
                    .iter()
                    .any(|executable| running.contains(executable))
            })
            .map(|game| game.name.clone())
            .collect()
    }
}

pub fn matches_running<'a, I>(games: &[GameConfig], executables: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let running: BTreeSet<String> = executables
        .into_iter()
        .map(str::to_ascii_lowercase)
        .collect();
    games
        .iter()
        .filter(|game| {
            game.executables
                .iter()
                .any(|executable| running.contains(executable))
        })
        .map(|game| game.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_case_insensitive_and_deduplicated_by_game() {
        let games = vec![GameConfig {
            name: "Counter-Strike 2".into(),
            executables: vec!["cs2.exe".into(), "csgo.exe".into()],
        }];
        assert_eq!(
            matches_running(&games, ["explorer.exe", "CS2.EXE"]),
            ["Counter-Strike 2"]
        );
    }

    #[test]
    fn desktop_catalog_shape_ignores_metadata() {
        let games = parse_catalog(
            br#"{
                "games": [{
                    "id": "counter-strike-2",
                    "name": "Counter-Strike 2",
                    "steamAppId": 730,
                    "executables": ["cs2.exe", "cs2"]
                }]
            }"#,
        )
        .expect("Desktop catalog should parse");

        assert_eq!(
            games,
            [GameConfig {
                name: "Counter-Strike 2".into(),
                executables: vec!["cs2.exe".into(), "cs2".into()],
            }]
        );
    }

    #[test]
    fn empty_desktop_catalog_is_rejected() {
        assert!(parse_catalog(br#"{"games":[]}"#).is_err());
    }
}
