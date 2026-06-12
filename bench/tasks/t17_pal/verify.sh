#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from pal import is_palindrome
assert is_palindrome('Race car') is True
assert is_palindrome('hello') is False
print('VERIFY OK')
PYEOF
