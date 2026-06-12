#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from names import initials
assert initials('ada lovelace') == 'AL'
assert initials('grace') == 'G'
print('VERIFY OK')
PYEOF
