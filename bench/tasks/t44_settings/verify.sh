#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from settings import effective
e = effective({'theme': 'dark'})
assert e['theme'] == 'dark'
assert e['font'] == 'mono'
print('VERIFY OK')
PYEOF
