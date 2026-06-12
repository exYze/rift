#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
import json, tempfile, os
from cfg import read_config
assert read_config('/nonexistent-rift-bench-xyz.json') == {}
p = tempfile.mktemp(suffix='.json')
with open(p, 'w') as f:
    json.dump({'a': 1}, f)
assert read_config(p) == {'a': 1}
os.unlink(p)
print('VERIFY OK')
PYEOF
