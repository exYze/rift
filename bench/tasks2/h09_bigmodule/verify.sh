#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
import copy
import configlib

snapshot = copy.deepcopy(configlib.DEFAULTS)

# Overlays win, defaults survive untouched.
cfg = configlib.apply_profile({"logging": {"level": "debug"},
                               "cache": {"ttl_seconds": 5}})
assert cfg["logging"]["level"] == "debug"
assert cfg["cache"]["ttl_seconds"] == 5
assert cfg["logging"]["enabled"] is True
assert configlib.DEFAULTS == snapshot, "DEFAULTS mutated by apply_profile"

cfg2 = configlib.apply_profile({"server": {"port": 9999}})
assert cfg2["logging"]["level"] == "info", cfg2["logging"]
assert cfg2["cache"]["ttl_seconds"] == 300, cfg2["cache"]
assert cfg2["server"]["port"] == 9999
assert configlib.DEFAULTS == snapshot

# deep_merge semantics: recursive, override wins, inputs untouched.
base = {"x": {"y": 1}, "keep": [1, 2]}
override = {"x": {"z": 2}, "new": 3}
merged = configlib.deep_merge(base, override)
assert merged == {"x": {"y": 1, "z": 2}, "keep": [1, 2], "new": 3}, merged
assert base == {"x": {"y": 1}, "keep": [1, 2]}, base
assert override == {"x": {"z": 2}, "new": 3}

# Stacked profiles behave and stay clean too.
stacked = configlib.apply_profiles([{"auth": {"mfa_required": True}},
                                    {"auth": {"session_minutes": 5}}])
assert stacked["auth"]["mfa_required"] is True
assert stacked["auth"]["session_minutes"] == 5
assert stacked["auth"]["provider"] == "local"
assert configlib.DEFAULTS == snapshot

# Nothing else was broken.
assert configlib.validate_all(configlib.DEFAULTS) == []
assert configlib.get_path(configlib.DEFAULTS, "database.port") == 5432
print("VERIFY OK")
PYEOF
