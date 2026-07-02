#!/usr/bin/env bash
set -e
# Tests must not have been modified.
if git ls-files --error-unmatch test_duration.py >/dev/null 2>&1; then
    git diff --quiet HEAD -- test_duration.py
else
    python3 - <<'PYEOF'
import hashlib, sys
h = hashlib.sha256(open("test_duration.py", "rb").read()).hexdigest()
sys.exit(0 if h == "7c5a5296a965540442ce63cae571babb861d0391ef78fc95d77ab3592a5362ae" else 1)
PYEOF
fi
python3 -m unittest discover -v
