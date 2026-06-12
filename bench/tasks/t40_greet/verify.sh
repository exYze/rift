#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from api import greet
assert greet({'first': 'Ada', 'last': 'Lovelace'}) == 'Hello, Ada Lovelace!'
print('VERIFY OK')
PYEOF
