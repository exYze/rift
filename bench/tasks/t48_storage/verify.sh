#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
import storage
assert storage.save('a', 1) is True
assert storage.load('a') == 1
assert storage.load('missing', 42) == 42
assert storage.load('missing') is None
print('VERIFY OK')
PYEOF
