#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from report import total_area
assert round(total_area([('rect', 2, 3), ('circle', 1)]), 2) == 9.14, total_area([('rect', 2, 3), ('circle', 1)])
print('VERIFY OK')
PYEOF
