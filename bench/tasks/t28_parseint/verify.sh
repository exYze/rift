#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from nums import parse_int
assert parse_int(' 42 ') == 42
assert parse_int('abc') is None
print('VERIFY OK')
PYEOF
