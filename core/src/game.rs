//! Detects whether Big Walk is running on this machine.

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

pub struct GameDetector {
    sys: System,
}

impl Default for GameDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl GameDetector {
    pub fn new() -> Self {
        Self {
            sys: System::new_with_specifics(RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing())),
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        self.sys
            .processes()
            .values()
            .any(|p| is_big_walk_process(&p.name().to_string_lossy()))
    }
}

/// `Big Walk.exe`, `Big Walk`, `BigWalk`... but not `Walk Relay` or a crash handler.
pub fn is_big_walk_process(name: &str) -> bool {
    let norm: String = name
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    norm == "bigwalk"
}

#[cfg(test)]
mod tests {
    use super::is_big_walk_process;

    #[test]
    fn matches_game_only() {
        assert!(is_big_walk_process("Big Walk.exe"));
        assert!(is_big_walk_process("Big Walk"));
        assert!(is_big_walk_process("BigWalk.exe"));
        assert!(is_big_walk_process("big_walk"));
        assert!(!is_big_walk_process("Walk Relay.exe"));
        assert!(!is_big_walk_process("walk-relay"));
        assert!(!is_big_walk_process("UnityCrashHandler64.exe"));
        assert!(!is_big_walk_process("Big Walk Helper"));
    }
}
