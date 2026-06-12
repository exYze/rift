#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from text import snake_to_title
assert snake_to_title('user_name') == 'User Name'
assert snake_to_title('a_b_c') == 'A B C'
print('VERIFY OK')
PYEOF
