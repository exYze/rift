#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from stats import median
assert median([1, 2, 3, 4]) == 2.5
assert median([5, 1, 3]) == 3
print('VERIFY OK')
PYEOF
