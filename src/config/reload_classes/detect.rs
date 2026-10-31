//! Change detectors for rows the serialized snapshot cannot compare.

use std::collections::BTreeMap;

use crate::config::Config;
use crate::services::tui_o::channel_policy;

/// `server.auth_token` is `skip_serializing`; compare it here, report only the path.
pub(super) fn server_auth_token(old: &Config, new: &Config) -> bool {
    old.server.auth_token != new.server.auth_token
}

/// Bot tokens are `skip_serializing`; compare them here, report only the path.
pub(super) fn discord_bot_tokens(old: &Config, new: &Config) -> bool {
    let tokens = |config: &Config| {
        config
            .discord
            .bots
            .iter()
            .map(|(name, bot)| (name.clone(), bot.token.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    tokens(old) != tokens(new)
}

/// Compared with the boot selection, so an edit and its revert balance out;
/// `channels` and `all_tui` both resolve to the same selected set.
pub(super) fn tui_o_writer_channels(old: &Config, new: &Config) -> bool {
    channel_policy::boot()
        .map(|boot| boot.selected().clone())
        .unwrap_or_else(|| channel_policy::configured_channels(old))
        != channel_policy::configured_channels(new)
}

pub(super) fn tui_o_writer_dormant_channels(old: &Config, new: &Config) -> bool {
    channel_policy::boot()
        .map(|boot| boot.dormant_channels().clone())
        .unwrap_or_else(|| channel_policy::configured_dormant_channels(old))
        != channel_policy::configured_dormant_channels(new)
}

/// A live ON cannot install reservations missed at boot, so it is reported on every reload.
pub(super) fn tui_o_writer_late_adopt(old: &Config, new: &Config) -> bool {
    let capable = channel_policy::boot()
        .map(|boot| boot.late_adopt_capable())
        .unwrap_or_else(|| channel_policy::tui_o_of(old).writer.late_adopt());
    channel_policy::tui_o_of(new).writer.late_adopt() && !capable
}
