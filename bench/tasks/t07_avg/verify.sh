#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from calc import average
assert average([]) == 0.0
assert average([2, 4]) == 3.0
print('VERIFY OK')
PYEOF
