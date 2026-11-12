"""Explicit, non-secret feature profiles; no ambient ADK environment copying."""

import json
import re

from .files import InstanceError

ENV_FLAGS = {
    "ADK_BUSY_INJECT": {"off", "external", "all"},
    "ADK_QUEUED_HEAD_INJECT": {"0", "1"},
    "ADK_TURN_PRESENCE_TAIL": {"0", "1"},
    "ADK_INTAKE_ROUTING_MODE": {"disabled", "observe", "enforce"},
    "ADK_IMSG_OBLIGATIONS": {"0"},
}
CHANNEL_FLAGS = {"ADK_BUSY_INJECT_DISCORD_CHANNELS", "ADK_BUSY_INJECT_STASH_CHANNELS"}
FEATURE_KEYS = {"env", "tui_writer", "tui_turn", "capacity_aware", "preferred_node"}


class Unevaluable(InstanceError):
    """The requested behavior cannot be evaluated without its preconditions."""


def exact_keys(value, keys):
    if not isinstance(value, dict) or set(value) != set(keys):
        raise InstanceError("invalid declaration fields")


def same_json(left, right):
    return json.dumps(left, sort_keys=True, allow_nan=False) == json.dumps(right, sort_keys=True, allow_nan=False)


def snowflake(value):
    if not isinstance(value, str) or not re.fullmatch(r"[1-9][0-9]{0,19}", value):
        raise InstanceError("test IDs must be positive u64 strings")
    if int(value) > 2**64 - 1:
        raise InstanceError("test ID exceeds u64")
    return value


def validate_profile(profile):
    if isinstance(profile, dict) and set(profile) == {"schema_version", "name", "bots", "channels"}:
        raise Unevaluable("missing feature declarations")
    exact_keys(profile, {"schema_version", "name", "bots", "channels", "features"})
    if type(profile["schema_version"]) is not int or profile["schema_version"] != 1 or profile["name"] != "tui-live":
        raise InstanceError("unsupported profile")
    exact_keys(profile["bots"], {"claude", "codex", "driver"})
    bot_ids = [snowflake(value) for value in profile["bots"].values()]
    channels = profile["channels"]
    if not isinstance(channels, list) or len(channels) != 2:
        raise InstanceError("exactly two test channels required")
    channels = [snowflake(value) for value in channels]
    if len(set(bot_ids + channels)) != 5:
        raise InstanceError("test IDs must be distinct")
    features = profile["features"]
    if not isinstance(features, dict) or set(features) - FEATURE_KEYS:
        raise InstanceError("invalid feature fields")
    if set(features) != FEATURE_KEYS:
        raise Unevaluable("missing feature declarations")
    flags = features["env"]
    if not isinstance(flags, dict) or set(flags) - (ENV_FLAGS.keys() | CHANNEL_FLAGS):
        raise InstanceError("feature environment key is not allowed")
    if set(flags) != ENV_FLAGS.keys() | CHANNEL_FLAGS:
        raise Unevaluable("missing feature flags")
    applied = {}
    for key, allowed in ENV_FLAGS.items():
        if not isinstance(flags[key], str) or flags[key] not in allowed:
            raise InstanceError("invalid feature flag value")
        applied[key] = flags[key]
    for key in sorted(CHANNEL_FLAGS):
        selected = flags[key]
        if not isinstance(selected, list) or len(set(map(str, selected))) != len(selected):
            raise InstanceError("invalid feature channel selection")
        if any(snowflake(value) not in channels for value in selected):
            raise InstanceError("feature channel outside test allowlist")
        applied[key] = ",".join(selected)
    for key in ("tui_writer", "tui_turn", "capacity_aware"):
        if type(features[key]) is not bool:
            raise InstanceError("feature switches must be explicit booleans")
    if features["preferred_node"] != "local":
        raise InstanceError("only the instance local node may be preferred")
    return applied


def require_preconditions(manifest, witness):
    """Require observed flags, host and local node before evaluating a scenario."""
    if not isinstance(witness, dict):
        raise Unevaluable("missing precondition witness")
    expected = {
        "instance_id": manifest["instance_id"],
        "head": manifest["head"],
        "applied_env": manifest["applied_env"],
        "features": manifest["profile"]["features"],
        "host": "local",
        "execution_node": manifest["execution_node_id"],
    }
    if not same_json(witness, expected):
        raise Unevaluable("precondition witness mismatch")
    features = manifest["profile"]["features"]
    if (not all(features[key] for key in ("tui_writer", "tui_turn", "capacity_aware"))
            or manifest["applied_env"]["ADK_BUSY_INJECT"] == "off"
            or manifest["applied_env"]["ADK_INTAKE_ROUTING_MODE"] != "enforce"
            or manifest["applied_env"]["ADK_QUEUED_HEAD_INJECT"] != "1"
            or manifest["applied_env"]["ADK_TURN_PRESENCE_TAIL"] != "1"
            or set(manifest["applied_env"]["ADK_BUSY_INJECT_DISCORD_CHANNELS"].split(","))
            != set(manifest["profile"]["channels"])):
        raise Unevaluable("required feature gate is off")
