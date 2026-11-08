//! How often the desktop app syncs. Every sync costs a few billable bucket
//! operations, so stay quick only while something is actually happening.

use std::time::Duration;

/// While Big Walk is open, a friend is playing, or a save just changed.
pub const ACTIVE_EVERY: Duration = Duration::from_secs(30);
/// When nobody is playing. A friend starting a session is noticed within this.
pub const IDLE_EVERY: Duration = Duration::from_secs(5 * 60);
/// Stay quick for this long after Big Walk closes here.
pub const STAY_ACTIVE_AFTER_EXIT: Duration = Duration::from_secs(10 * 60);

/// Time until the next regular sync.
///
/// * `game_running`: Big Walk is open on this computer.
/// * `friend_playing`: someone else has Big Walk open or is hosting.
/// * `since_exit`: time since Big Walk last closed here, if it has.
/// * `all_in_sync`: every journey on this computer matches the bucket.
pub fn sync_interval(game_running: bool, friend_playing: bool, since_exit: Option<Duration>, all_in_sync: bool) -> Duration {
    let just_played = since_exit.is_some_and(|d| d < STAY_ACTIVE_AFTER_EXIT);
    if game_running || friend_playing || just_played || !all_in_sync {
        ACTIVE_EVERY
    } else {
        IDLE_EVERY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_only_while_something_is_happening() {
        assert_eq!(sync_interval(false, false, None, true), IDLE_EVERY);
        assert_eq!(sync_interval(true, false, None, true), ACTIVE_EVERY);
        assert_eq!(sync_interval(false, true, None, true), ACTIVE_EVERY);
        assert_eq!(sync_interval(false, false, None, false), ACTIVE_EVERY);
        assert_eq!(sync_interval(false, false, Some(Duration::from_secs(60)), true), ACTIVE_EVERY);
        assert_eq!(sync_interval(false, false, Some(Duration::from_secs(11 * 60)), true), IDLE_EVERY);
    }
}
