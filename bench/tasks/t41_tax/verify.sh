#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from cart import total
assert total(100) == 108.0, total(100)
assert total(50) == 54.0
print('VERIFY OK')
PYEOF
