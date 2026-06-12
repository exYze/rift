#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from dur import humanize
assert humanize(3661) == '1h 1m 1s'
assert humanize(61) == '1m 1s'
assert humanize(5) == '5s'
assert humanize(3600) == '1h 0m 0s'
print('VERIFY OK')
PYEOF
