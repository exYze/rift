#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
import orders, report
assert hasattr(orders, 'calculate_total')
assert not hasattr(orders, 'calc')
assert orders.calculate_total([2, 3]) == 5
assert report.summary([2, 3]) == 'total: 5'
print('VERIFY OK')
PYEOF
