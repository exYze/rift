#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from freq import counts
assert counts('a b a') == {'a': 2, 'b': 1}
assert counts('') == {}
print('VERIFY OK')
PYEOF
