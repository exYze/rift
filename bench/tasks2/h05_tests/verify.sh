#!/usr/bin/env bash
set -e
# Tests must not have been modified.
if git ls-files --error-unmatch test_booking.py >/dev/null 2>&1; then
    git diff --quiet HEAD -- test_booking.py
else
    python3 - <<'PYEOF'
import hashlib, sys
h = hashlib.sha256(open("test_booking.py", "rb").read()).hexdigest()
sys.exit(0 if h == "582c6389c3149143243ad07046634b48e678117a84f74e4b823e77a8b0b19f5c" else 1)
PYEOF
fi
python3 -m unittest discover -v
