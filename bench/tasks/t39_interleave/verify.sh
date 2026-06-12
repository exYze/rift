#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from weave import interleave
assert interleave([1, 2, 3], ['a']) == [1, 'a', 2, 3]
assert interleave([], [1]) == [1]
assert interleave([1], []) == [1]
print('VERIFY OK')
PYEOF
