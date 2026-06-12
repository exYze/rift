#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from slug import slugify
assert slugify('Hello, World!') == 'hello-world'
assert slugify('A  B') == 'a-b'
assert slugify('--Already-Slug--') == 'already-slug'
print('VERIFY OK')
PYEOF
