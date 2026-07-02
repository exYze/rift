#!/usr/bin/env bash
set -e
# Tests must not have been modified.
if git ls-files --error-unmatch test_lru.py >/dev/null 2>&1; then
    git diff --quiet HEAD -- test_lru.py
else
    python3 - <<'PYEOF'
import hashlib, sys
h = hashlib.sha256(open("test_lru.py", "rb").read()).hexdigest()
sys.exit(0 if h == "f3791ece20cf1550cb79c706b502dfe5f934e2df2d44be87fcb9624f5af434ce" else 1)
PYEOF
fi
python3 -m unittest discover -v
