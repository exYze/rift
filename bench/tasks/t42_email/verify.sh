#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from forms import validate
assert validate('a@b.co') is True
assert validate('nope') is False
assert validate('@b.co') is False
assert validate('a@bco') is False
print('VERIFY OK')
PYEOF
