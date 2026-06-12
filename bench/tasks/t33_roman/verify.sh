#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from roman import int_to_roman
assert int_to_roman(4) == 'IV'
assert int_to_roman(9) == 'IX'
assert int_to_roman(14) == 'XIV'
assert int_to_roman(40) == 'XL'
assert int_to_roman(49) == 'XLIX'
assert int_to_roman(100) == 'C'
print('VERIFY OK')
PYEOF
