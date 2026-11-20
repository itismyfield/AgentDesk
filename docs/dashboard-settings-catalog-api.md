# Dashboard settings catalog API

`GET /api/settings/config` returns `{entries: [... ]}`. Each entry has these fields.

| Field | JSON type | Meaning |
| --- | --- | --- |
| `key` | string | Allowlisted config key |
| `category` | string | Existing group ID from `pipeline`, `quality`, `review`, `system`, `timeout`, or `context` |
| `label_ko`, `label_en`, `description_ko` | string | Display labels and Korean description |
| `value` | string or null | Effective value |
| `default`, `baseline` | string or null | Existing default and baseline |
| `baseline_source` | string or null | `config`, `yaml`, or `hardcoded` when applicable |
| `override_active`, `editable` | boolean | Live override and editability indicators |
| `restart_behavior` | string | `config-only`, `reset-to-baseline`, `clear-on-restart`, `reseed-from-yaml`, or `persist-live-override` |

`GET /api/settings/runtime-config` returns `current` and `defaults` as objects,
`explicit_keys` as an array of strings, and `key_metadata` as an object keyed by
the supported camelCase setting names. Each `key_metadata` entry is
`{"label_ko":"…"}`. Legacy keys may appear in `current` without metadata.
Build a PUT body from the selected values and any explicit-keys metadata required
by the existing write contract. `key_metadata` is for display.

The descriptions and labels come from static tables. Both schemas appear at
`GET /api/docs/config/settings`. See [settings precedence](adr-settings-precedence.md)
for value resolution and restart behavior.
