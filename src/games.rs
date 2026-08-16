use std::collections::BTreeSet;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::GameConfig;

const BUILT_IN_CATALOG: &str = include_str!("../games.json");

// Entries shipped in older catalogs that should no longer be seeded. Keeping
// this migration list prevents them from living forever in existing configs.
const RETIRED_BUILT_IN_GAMES: &[&str] = &[
    "Grand Theft Auto V",
    "Roblox",
    "Destiny 2",
    "Rust",
    "Helldivers 2",
    "Minecraft for Windows",
    "Rocket League",
    "War Thunder",
    "Dead by Daylight",
    "Halo Infinite",
    "Street Fighter 6",
    "Tekken 8",
    "StarCraft II",
];

pub fn catalog() -> Vec<GameConfig> {
    serde_json::from_str(BUILT_IN_CATALOG)
        .expect("the built-in games.json catalog must contain valid game entries")
}

pub fn merge_catalog(games: &mut Vec<GameConfig>) {
    games.retain(|game| {
        !RETIRED_BUILT_IN_GAMES
            .iter()
            .any(|retired| game.name.eq_ignore_ascii_case(retired))
    });
    for catalog_game in catalog() {
        if let Some(existing) = games
            .iter_mut()
            .find(|game| game.name.eq_ignore_ascii_case(&catalog_game.name))
        {
            existing.executables.extend(catalog_game.executables);
        } else {
            games.push(catalog_game);
        }
    }
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
    fn built_in_catalog_contains_requested_games() {
        let games = catalog();
        for name in [
            "Rainbow Six Siege",
            "Valorant",
            "Overwatch 2",
            "Apex Legends",
            "Counter-Strike 2",
            "Marvel Rivals",
            "Fortnite",
            "Escape from Tarkov",
        ] {
            assert!(games.iter().any(|game| game.name == name), "missing {name}");
        }
    }

    #[test]
    fn retired_catalog_entries_are_removed_but_custom_games_survive() {
        let mut games = vec![
            GameConfig {
                name: "Minecraft for Windows".into(),
                executables: vec!["Minecraft.Windows.exe".into()],
            },
            GameConfig {
                name: "Custom Arena".into(),
                executables: vec!["arena.exe".into()],
            },
        ];

        merge_catalog(&mut games);

        assert!(
            !games
                .iter()
                .any(|game| game.name == "Minecraft for Windows")
        );
        assert!(games.iter().any(|game| game.name == "Custom Arena"));
    }
}
