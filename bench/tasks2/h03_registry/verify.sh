#!/usr/bin/env bash
set -e
out="$(python3 app.py)"
echo "$out" | grep -q "boot ok:"
python3 - <<'PYEOF'
from registry import Registry
from dispatch import setup_all

# Diamond dependency: a <- b, a <- c, {b, c} <- d, registered worst-first.
r = Registry()
calls = []

def mk(n):
    def setup():
        calls.append(n)
    return setup

r.register("d", requires=("b", "c"))(mk("d"))
r.register("b", requires=("a",))(mk("b"))
r.register("c", requires=("a",))(mk("c"))
r.register("a")(mk("a"))

order = setup_all(r)
assert sorted(order) == ["a", "b", "c", "d"], order
assert calls == order, (calls, order)
pos = {n: i for i, n in enumerate(order)}
assert pos["a"] < pos["b"], order
assert pos["a"] < pos["c"], order
assert pos["b"] < pos["d"], order
assert pos["c"] < pos["d"], order

# Setup functions run exactly once each.
assert len(calls) == 4, calls
print("VERIFY OK")
PYEOF
