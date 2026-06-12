#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from brackets import balanced
assert balanced('([]{})') is True
assert balanced('(]') is False
assert balanced('(((') is False
assert balanced('') is True
print('VERIFY OK')
PYEOF
