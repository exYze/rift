#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from seq import chunk
assert chunk([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]]
assert chunk([1, 2], 2) == [[1, 2]]
print('VERIFY OK')
PYEOF
