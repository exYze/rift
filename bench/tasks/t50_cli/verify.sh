#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from cli import parse_args
assert parse_args(['--name', 'ada', '--loud']) == {'name': 'ada', 'loud': True}
assert parse_args([]) == {'name': None, 'loud': False}
assert parse_args(['--loud', '--name', 'x']) == {'name': 'x', 'loud': True}
print('VERIFY OK')
PYEOF
