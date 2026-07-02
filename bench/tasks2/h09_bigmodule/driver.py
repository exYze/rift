"""Repro: loading a profile must not corrupt the shared DEFAULTS."""
import copy

import configlib


def main():
    snapshot = copy.deepcopy(configlib.DEFAULTS)

    cfg = configlib.apply_profile({
        "logging": {"level": "debug"},
        "cache": {"ttl_seconds": 5},
    })
    assert cfg["logging"]["level"] == "debug"
    assert cfg["cache"]["ttl_seconds"] == 5
    assert configlib.DEFAULTS == snapshot, (
        "apply_profile polluted configlib.DEFAULTS: "
        f"logging={configlib.DEFAULTS['logging']}"
    )

    # A second, independent load must start from clean defaults.
    cfg2 = configlib.apply_profile({"server": {"port": 9999}})
    assert cfg2["logging"]["level"] == "info", cfg2["logging"]
    assert cfg2["cache"]["ttl_seconds"] == 300, cfg2["cache"]
    assert configlib.DEFAULTS == snapshot

    # Merging must never touch its inputs, nested dicts included.
    base = {"x": {"y": 1}, "keep": [1, 2]}
    override = {"x": {"z": 2}}
    merged = configlib.deep_merge(base, override)
    assert merged == {"x": {"y": 1, "z": 2}, "keep": [1, 2]}, merged
    assert base == {"x": {"y": 1}, "keep": [1, 2]}, (
        f"deep_merge mutated its base argument: {base}"
    )
    assert override == {"x": {"z": 2}}

    print("DRIVER OK")


if __name__ == "__main__":
    main()
