#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from rle import encode
assert encode('aaabb') == 'a3b2'
assert encode('abc') == 'a1b1c1'
assert encode('') == ''
print('VERIFY OK')
PYEOF
