#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from app import dispatch

# Working routes stay working.
assert dispatch("/users/7") == (200, "user Alice"), dispatch("/users/7")
assert dispatch("/users/12") == (200, "user Bob")
assert dispatch("/orders/12") == (200, "order 3 widgets")
assert dispatch("/orders/31") == (200, "order 1 gadget")
assert dispatch("/health") == (200, "ok")
assert dispatch("/users/99")[0] == 404

# Extra trailing segments must not match a shorter route.
assert dispatch("/users/7/extra")[0] == 404, dispatch("/users/7/extra")
assert dispatch("/orders/12/x/y")[0] == 404, dispatch("/orders/12/x/y")
assert dispatch("/health/z")[0] == 404

# <int:...> params are plain non-negative decimals only.
assert dispatch("/orders/-2")[0] == 404, dispatch("/orders/-2")
assert dispatch("/users/-1")[0] == 404
assert dispatch("/orders/1.5")[0] == 404
assert dispatch("/users/abc")[0] == 404
assert dispatch("/nope")[0] == 404
print("VERIFY OK")
PYEOF
