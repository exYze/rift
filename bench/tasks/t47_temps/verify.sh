#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from temps import c_to_f, f_to_c
assert f_to_c(c_to_f(25)) == 25.0
assert f_to_c(32) == 0.0
assert f_to_c(212) == 100.0
print('VERIFY OK')
PYEOF
