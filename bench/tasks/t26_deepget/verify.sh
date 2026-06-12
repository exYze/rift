#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from dicts import deep_get
assert deep_get({'a': {'b': 5}}, 'a.b') == 5
assert deep_get({}, 'a.b', 0) == 0
assert deep_get({'a': 1}, 'a.b', 'x') == 'x'
print('VERIFY OK')
PYEOF
