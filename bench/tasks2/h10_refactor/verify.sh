#!/usr/bin/env bash
set -e
[ "$(python3 main.py | tail -n 1)" = "REFACTOR OK" ]
python3 - <<'PYEOF'
import inspect

import store
import inventory
import reports
import cli

# New API present, old name gone.
assert hasattr(store, "fetch_items"), "fetch_items missing"
assert not hasattr(store, "load_items"), "old name load_items still present"
sig = inspect.signature(store.fetch_items)
assert "limit" in sig.parameters, sig
assert sig.parameters["limit"].default is None, sig

# New behaviour.
assert store.fetch_items("hardware", limit=3) == [
    ("bolt", 250), ("nut", 900), ("screw", 610)]
assert store.fetch_items("hardware", limit=99) == [
    ("bolt", 250), ("nut", 900), ("screw", 610), ("washer", 40)]
assert store.fetch_items("produce") == [("apple", 30), ("pear", 12), ("plum", 55)]
assert store.fetch_items("nope", limit=2) == []

# Every caller moved over and still behaves (these exercise each module's
# internal calls into the store, so a stale load_items call would blow up).
assert inventory.total_qty("hardware") == 1800
assert inventory.kinds("stationery") == 2
assert inventory.low_stock("produce", 20) == ["pear"]
assert reports.summary("produce") == "produce: 3 kinds, 97 units"
assert reports.first_rows("produce", 2) == [("apple", 30), ("pear", 12)]
assert reports.full_report(["stationery"]) == "stationery: 2 kinds, 165 units"
assert cli.run([]) == 0
assert cli.run(["stationery"]) == 1
print("VERIFY OK")
PYEOF
