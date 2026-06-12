#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from seq import flatten
assert flatten([[1, 2], [3]]) == [1, 2, 3]
assert flatten([]) == []
print('VERIFY OK')
PYEOF
