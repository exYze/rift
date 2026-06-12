#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from text import truncate
assert truncate('hello world', 8) == 'hello w…', repr(truncate('hello world', 8))
assert truncate('hi', 8) == 'hi'
print('VERIFY OK')
PYEOF
