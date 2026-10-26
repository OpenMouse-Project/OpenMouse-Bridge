use std::{borrow::Cow, collections::BTreeSet, env, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[cfg(test)]
use crate::config::LocalApplicationKind;
use crate::config::{GameConfig, LocalApplication};

pub const GAMES_CDN_URL: &str =
    "https://cdn.jsdelivr.net/gh/OpenMouse-Project/Desktop@main/public/games.json";
const GAMES_URL_ENV: &str = "OPENMOUSE_BRIDGE_GAMES_URL";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_CATALOG_GAMES: usize = 10_000;
pub(crate) const MAX_EXECUTABLES: usize = 32;
pub(crate) const MAX_CATALOG_STRING: usize = 256;

#[derive(Deserialize)]
struct CatalogResponse {
    #[serde(deserialize_with = "deserialize_games")]
    games: Vec<GameConfig>,
}

#[derive(Deserialize)]
struct CatalogGame {
    name: String,
    #[serde(deserialize_with = "deserialize_executables")]
    executables: Vec<String>,
}

fn deserialize_executables<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    struct ExecutablesVisitor;
    impl<'de> serde::de::Visitor<'de> for ExecutablesVisitor {
        type Value = Vec<String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a bounded array of executable names")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut executables = Vec::new();
            while let Some(entry) = seq.next_element::<serde_json::Value>()? {
                let serde_json::Value::String(name) = entry else {
                    continue;
                };
                if name.len() > MAX_CATALOG_STRING
                    || name.trim().is_empty()
                    || name.chars().any(char::is_control)
                {
                    continue;
                }
                if executables.len() >= MAX_EXECUTABLES {
                    return Err(serde::de::Error::custom(
                        "catalog executable count exceeds limit",
                    ));
                }
                executables.push(name);
            }
            Ok(executables)
        }
    }
    deserializer.deserialize_seq(ExecutablesVisitor)
}

fn deserialize_games<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<GameConfig>, D::Error> {
    struct GamesVisitor;
    impl<'de> serde::de::Visitor<'de> for GamesVisitor {
        type Value = Vec<GameConfig>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a bounded array of game entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut games = Vec::new();
            let mut game_count = 0;
            while let Some(game) = seq.next_element::<CatalogGame>()? {
                if game_count >= MAX_CATALOG_GAMES
                    || game.name.len() > MAX_CATALOG_STRING
                    || game.name.trim().is_empty()
                    || game.name.chars().any(char::is_control)
                {
                    return Err(serde::de::Error::custom(
                        "game catalog exceeds item limits or contains invalid strings",
                    ));
                }
                game_count += 1;
                if !game.executables.is_empty() {
                    games.push(GameConfig {
                        name: game.name,
                        executables: game.executables,
                    });
                }
            }
            Ok(games)
        }
    }
    deserializer.deserialize_seq(GamesVisitor)
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
    let response = client
        .get(url.as_ref())
        .send()
        .await
        .with_context(|| format!("could not download {url}"))?
        .error_for_status()
        .with_context(|| format!("game catalog request failed for {url}"))?;
    let bytes = crate::updater::read_bounded_response(response, MAX_CATALOG_BYTES).await?;
    parse_catalog(&bytes)
}

fn parse_catalog(bytes: &[u8]) -> Result<Vec<GameConfig>> {
    ensure!(
        bytes.len() <= MAX_CATALOG_BYTES,
        "game catalog exceeds byte limit"
    );
    let catalog: CatalogResponse =
        serde_json::from_slice(bytes).context("could not parse the game catalog")?;
    ensure!(!catalog.games.is_empty(), "game catalog contains no games");
    Ok(catalog.games)
}

pub struct GameDetector {
    system: System,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunningProcess {
    pub executable: String,
    pub path: String,
}

pub fn matches_local_games(
    targets: &[LocalApplication],
    processes: &[RunningProcess],
) -> Vec<String> {
    targets
        .iter()
        .filter(|target| {
            target.effective_activation() == crate::config::LocalApplicationActivation::Running
                && !target.path.is_empty()
                && processes.iter().any(|process| {
                    matches_target(
                        &target.path,
                        &target.executable,
                        &process.path,
                        &process.executable,
                    )
                })
        })
        .map(|target| target.id.clone())
        .collect()
}

pub fn path_key(path: &str) -> String {
    if is_windows_path(path) {
        path.replace('\\', "/").to_lowercase()
    } else {
        path.to_owned()
    }
}

pub fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

pub fn matches_target(
    path: &str,
    executable: &str,
    running_path: &str,
    running_executable: &str,
) -> bool {
    if !path.is_empty() {
        !running_path.is_empty() && path_key(path) == path_key(running_path)
    } else {
        !executable.is_empty() && executable.eq_ignore_ascii_case(running_executable)
    }
}

impl Default for GameDetector {
    fn default() -> Self {
        Self {
            system: System::new(),
        }
    }
}

impl GameDetector {
    pub fn running_processes(&self) -> Vec<RunningProcess> {
        self.system
            .processes()
            .values()
            .filter_map(|process| {
                let path = process.exe()?;
                let executable = path.file_name()?.to_string_lossy().into_owned();
                Some(RunningProcess {
                    executable,
                    path: path.to_string_lossy().into_owned(),
                })
            })
            .collect()
    }

    pub fn detect(&mut self, games: &[GameConfig]) -> Vec<String> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
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
    fn running_apps_use_inventory_and_missing_paths_fail_closed() {
        let mut app: LocalApplication = serde_json::from_value(serde_json::json!({
            "id":"local:editor", "name":"Editor", "kind":"app", "activation":"running",
            "executable":"editor", "path":"/opt/editor", "bundleIdentifier":"io.editor"
        }))
        .unwrap();
        let running = RunningProcess {
            executable: "editor".into(),
            path: "/opt/editor".into(),
        };
        assert_eq!(
            matches_local_games(std::slice::from_ref(&app), std::slice::from_ref(&running)),
            ["local:editor"]
        );
        app.path.clear();
        assert!(
            matches_local_games(std::slice::from_ref(&app), std::slice::from_ref(&running))
                .is_empty()
        );
        app.path = "/other/editor".into();
        assert!(matches_local_games(std::slice::from_ref(&app), &[running]).is_empty());
        app.path = "/opt/editor".into();
        app.activation = Some(crate::config::LocalApplicationActivation::Focused);
        assert!(
            matches_local_games(
                &[app],
                &[RunningProcess {
                    executable: "editor".into(),
                    path: "/opt/editor".into()
                }]
            )
            .is_empty()
        );
    }

    #[test]
    fn windows_paths_ignore_unicode_case_but_posix_paths_do_not() {
        assert!(matches_target(
            "C:/Tools/Éditor.exe",
            "Éditor.exe",
            r"c:\tools\éditor.exe",
            "éditor.exe"
        ));
        assert!(!matches_target(
            "/Tools/Éditor",
            "Éditor",
            "/Tools/éditor",
            "éditor"
        ));
    }

    #[test]
    fn local_games_detect_the_running_test_executable_without_window_enumeration() {
        let path = std::env::current_exe().unwrap();
        let target = LocalApplication {
            id: "local:test-process".into(),
            name: "No Window Test".into(),
            executable: path.file_name().unwrap().to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            kind: LocalApplicationKind::Game,
            activation: None,
            bundle_path: None,
            bundle_identifier: None,
        };
        let mut detector = GameDetector::default();
        detector.detect(&[]);
        assert_eq!(
            matches_local_games(&[target], &detector.running_processes()),
            ["local:test-process"]
        );
    }

    #[test]
    fn local_process_matching_is_path_strict_and_excludes_apps() {
        let target = LocalApplication {
            id: "local:game".into(),
            name: "Custom".into(),
            executable: "custom.exe".into(),
            path: "C:/Trusted/custom.exe".into(),
            kind: LocalApplicationKind::Game,
            activation: None,
            bundle_path: None,
            bundle_identifier: None,
        };
        let wrong = RunningProcess {
            executable: "custom.exe".into(),
            path: "C:/Other/custom.exe".into(),
        };
        assert!(matches_local_games(std::slice::from_ref(&target), &[wrong]).is_empty());
        let running = RunningProcess {
            executable: "CUSTOM.exe".into(),
            path: r"c:\trusted\CUSTOM.exe".into(),
        };
        assert_eq!(
            matches_local_games(
                std::slice::from_ref(&target),
                std::slice::from_ref(&running)
            ),
            ["local:game"]
        );
        let app = LocalApplication {
            kind: LocalApplicationKind::App,
            activation: None,
            bundle_path: None,
            bundle_identifier: None,
            ..target.clone()
        };
        assert!(matches_local_games(&[app], std::slice::from_ref(&running)).is_empty());
        let leaf = LocalApplication {
            path: String::new(),
            ..target
        };
        assert!(matches_local_games(&[leaf], &[running]).is_empty());
    }

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
    fn catalog_skips_invalid_executable_entries() {
        let catalog = serde_json::json!({"games":[{
            "name":"Mixed",
            "executables":["ok.exe", "", "   ", "bad\u{0001}.exe", "x".repeat(MAX_CATALOG_STRING + 1), null, false, 42, {}, ["nested"], "other.exe"]
        }]});
        let games = parse_catalog(&serde_json::to_vec(&catalog).unwrap()).unwrap();
        assert_eq!(games[0].executables, ["ok.exe", "other.exe"]);
    }

    #[test]
    fn catalog_drops_games_without_valid_executables() {
        let catalog = serde_json::json!({"games":[
            {"name":"Invalid", "executables":["", "\n", null]},
            {"name":"Empty", "executables":[]},
            {"name":"Valid", "executables":["valid.exe"]}
        ]});
        let games = parse_catalog(&serde_json::to_vec(&catalog).unwrap()).unwrap();
        assert_eq!(
            games,
            [GameConfig {
                name: "Valid".into(),
                executables: vec!["valid.exe".into()]
            }]
        );
    }

    #[test]
    fn catalog_game_limit_counts_discarded_entries() {
        let mut entries =
            vec![serde_json::json!({"name":"Invalid", "executables":[]}); MAX_CATALOG_GAMES];
        entries.push(serde_json::json!({"name":"Valid", "executables":["valid.exe"]}));
        let error =
            parse_catalog(&serde_json::to_vec(&serde_json::json!({"games":entries})).unwrap())
                .unwrap_err();
        assert!(format!("{error:#}").contains("game catalog exceeds item limits"));
    }

    #[test]
    fn catalog_limits_reject_bytes_counts_and_strings() {
        let oversized = serde_json::json!({"games":[{"name":"x","executables":["x"]}],"padding":"x".repeat(MAX_CATALOG_BYTES)});
        assert!(parse_catalog(&serde_json::to_vec(&oversized).unwrap()).is_err());
        for game in [
            serde_json::json!({"name":"x".repeat(257),"executables":["x"]}),
            serde_json::json!({"name":"x","executables":["x".repeat(257)]}),
            serde_json::json!({"name":"x","executables":vec!["x";33]}),
            serde_json::json!({"name":" ","executables":["x"]}),
            serde_json::json!({"name":"x","executables":[""]}),
        ] {
            assert!(
                parse_catalog(&serde_json::to_vec(&serde_json::json!({"games":[game]})).unwrap())
                    .is_err()
            );
        }
        let boundary =
            serde_json::json!({"name":"x".repeat(256),"executables":vec!["x".repeat(256);32]});
        assert!(
            parse_catalog(&serde_json::to_vec(&serde_json::json!({"games":[boundary]})).unwrap())
                .is_ok()
        );
        let game = serde_json::json!({"name":"x","executables":["x"]});
        assert_eq!(
            parse_catalog(
                &serde_json::to_vec(&serde_json::json!({"games":vec![game.clone();10000]}))
                    .unwrap()
            )
            .unwrap()
            .len(),
            10000
        );
        assert!(
            parse_catalog(
                &serde_json::to_vec(&serde_json::json!({"games":vec![game;10001]})).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn empty_desktop_catalog_is_rejected() {
        assert!(parse_catalog(br#"{"games":[]}"#).is_err());
    }
}
