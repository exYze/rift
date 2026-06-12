#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from cipher import caesar
assert caesar('abz', 1) == 'bca'
assert caesar('AbZ', 2) == 'CdB'
assert caesar('a-b', 1) == 'b-c'
print('VERIFY OK')
PYEOF
