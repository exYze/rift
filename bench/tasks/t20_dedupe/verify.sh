#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from seq import dedupe
assert dedupe([3, 1, 3, 2, 1]) == [3, 1, 2]
assert dedupe([]) == []
print('VERIFY OK')
PYEOF
