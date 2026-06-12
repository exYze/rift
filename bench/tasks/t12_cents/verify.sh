#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from till import total
assert total([0.1, 0.2]) == 0.3, total([0.1, 0.2])
assert total([1.005, 2.0]) == 3.01
print('VERIFY OK')
PYEOF
