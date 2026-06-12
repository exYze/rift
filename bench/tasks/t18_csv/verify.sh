#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from csvish import parse_line
assert parse_line('a, b , c') == ['a', 'b', 'c']
assert parse_line('x') == ['x']
print('VERIFY OK')
PYEOF
