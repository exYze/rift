#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from matrix import transpose
assert transpose([[1, 2], [3, 4], [5, 6]]) == [[1, 3, 5], [2, 4, 6]]
assert transpose([[7]]) == [[7]]
print('VERIFY OK')
PYEOF
