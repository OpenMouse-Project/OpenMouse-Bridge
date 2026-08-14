use std::collections::BTreeSet;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::GameConfig;

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
}
