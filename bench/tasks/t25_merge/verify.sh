#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from conf import merge
assert merge({'x': 1}, {'x': 2, 'y': 3}) == {'x': 2, 'y': 3}
print('VERIFY OK')
PYEOF
