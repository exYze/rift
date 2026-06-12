#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from dates import is_leap
assert is_leap(2024) is True
assert is_leap(1900) is False
assert is_leap(2000) is True
assert is_leap(2023) is False
print('VERIFY OK')
PYEOF
