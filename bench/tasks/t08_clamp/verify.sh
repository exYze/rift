#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from num import clamp
assert clamp(5, 0, 10) == 5
assert clamp(-3, 0, 10) == 0
assert clamp(99, 0, 10) == 10
print('VERIFY OK')
PYEOF
