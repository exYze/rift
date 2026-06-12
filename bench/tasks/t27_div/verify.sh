#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from math2 import safe_divide
assert safe_divide(6, 3) == 2.0
assert safe_divide(1, 0) is None
print('VERIFY OK')
PYEOF
