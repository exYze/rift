#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from rank import top_n
assert top_n([5, 1, 9, 3], 2) == [9, 5]
assert top_n([2], 1) == [2]
print('VERIFY OK')
PYEOF
