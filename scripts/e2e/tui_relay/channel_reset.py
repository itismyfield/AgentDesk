"""Shared host policy for E2E reset: a Herdr session is preserved."""


def preserve_herdr_session(*, channel_id, configured_channels, log):
    target = str(channel_id)
    if (not target.isascii() or not target.isdecimal()
            or int(target) <= 0 or str(int(target)) != target):
        raise ValueError("reset target channel is unreadable")
    if (not isinstance(configured_channels, list)
            or any(not isinstance(value, str) or not value.isascii() or not value.isdecimal()
                   or int(value) <= 0 or str(int(value)) != value
                   for value in configured_channels)):
        raise ValueError("Herdr channels mapping unreadable; reset refused")
    if target in configured_channels:
        log("skipped: configured channel is owned by Herdr")
        return True
    return False
