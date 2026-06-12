#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from words import pluralize
assert pluralize('box') == 'boxes'
assert pluralize('cat') == 'cats'
assert pluralize('church') == 'churches'
assert pluralize('dish') == 'dishes'
print('VERIFY OK')
PYEOF
