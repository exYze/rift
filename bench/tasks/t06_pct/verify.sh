#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from stats import pct
assert pct(1, 4) == 25.0, pct(1, 4)
assert pct(3, 4) == 75.0
assert pct(2, 2) == 100.0
print('VERIFY OK')
PYEOF
