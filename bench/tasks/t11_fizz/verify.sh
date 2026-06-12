#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
from fizz import fizzbuzz
assert fizzbuzz(15) == 'FizzBuzz'
assert fizzbuzz(9) == 'Fizz'
assert fizzbuzz(10) == 'Buzz'
assert fizzbuzz(7) == '7'
print('VERIFY OK')
PYEOF
