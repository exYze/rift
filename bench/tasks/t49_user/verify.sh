#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from main import make_users
us = make_users()
assert us[0].email == 'ada@x.io'
assert us[1].email == ''
assert us[1].name == 'grace'
print('VERIFY OK')
PYEOF
