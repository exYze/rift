#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from person import set_age
assert set_age(5) == 5
try:
    set_age(-1)
    raise SystemExit(1)
except ValueError:
    pass
print('VERIFY OK')
PYEOF
