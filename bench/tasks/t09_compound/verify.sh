#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from interest import compound
assert compound(1000, 0.10, 2) == 1210.0, compound(1000, 0.10, 2)
assert compound(100, 0.5, 1) == 150.0
print('VERIFY OK')
PYEOF
