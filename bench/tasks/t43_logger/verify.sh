#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from app import process
assert process('boom') == '[error] boom'
print('VERIFY OK')
PYEOF
