#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from retry import retry
calls = {'n': 0}
def flaky():
    calls['n'] += 1
    if calls['n'] < 3:
        raise RuntimeError('boom')
    return 'ok'
assert retry(flaky, 3) == 'ok'
assert calls['n'] == 3
try:
    retry(lambda: 1 // 0, 2)
    raise SystemExit(1)
except ZeroDivisionError:
    pass
print('VERIFY OK')
PYEOF
