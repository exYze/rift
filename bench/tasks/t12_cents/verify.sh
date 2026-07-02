#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from till import total
assert total([0.1, 0.2]) == 0.3, total([0.1, 0.2])
assert total([1.10, 2.24]) == 3.34, total([1.10, 2.24])
print('VERIFY OK')
PYEOF
