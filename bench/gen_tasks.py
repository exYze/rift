#!/usr/bin/env python3
"""Generate bench tasks t06-t50: 45 small, unambiguous coding tasks.

Each task = source file(s) with a planted bug (or missing implementation),
prompt.txt describing the symptom/requirement, and verify.sh that FAILS on
the broken fixture and passes once correctly fixed. Run this script once;
it also self-checks that every verify fails pre-fix.
"""
import os
import stat
import subprocess
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "tasks")

# (name, prompt, {filename: content}, verify_python_body)
TASKS = [
    # ---------- arithmetic / logic bugs ----------
    ("t06_pct",
     "pct(part, total) in stats.py is supposed to return the percentage as a float "
     "(e.g. pct(1, 4) == 25.0) but it returns 0. Fix it.",
     {"stats.py": "def pct(part, total):\n    return part // total * 100\n"},
     "from stats import pct\nassert pct(1, 4) == 25.0, pct(1, 4)\nassert pct(3, 4) == 75.0\nassert pct(2, 2) == 100.0\n"),

    ("t07_avg",
     "average(xs) in calc.py crashes with ZeroDivisionError when xs is empty. "
     "It should return 0.0 for an empty list and keep working normally otherwise.",
     {"calc.py": "def average(xs):\n    return sum(xs) / len(xs)\n"},
     "from calc import average\nassert average([]) == 0.0\nassert average([2, 4]) == 3.0\n"),

    ("t08_clamp",
     "clamp(v, lo, hi) in num.py should constrain v to the range [lo, hi], "
     "but it returns wrong values (clamp(5, 0, 10) returns 0). Fix it.",
     {"num.py": "def clamp(v, lo, hi):\n    return min(max(v, hi), lo)\n"},
     "from num import clamp\nassert clamp(5, 0, 10) == 5\nassert clamp(-3, 0, 10) == 0\nassert clamp(99, 0, 10) == 10\n"),

    ("t09_compound",
     "compound(principal, rate, years) in interest.py should compound annually: "
     "1000 at 10% for 2 years = 1210.0. It currently returns simple growth. Fix it "
     "(round the result to 2 decimals).",
     {"interest.py": "def compound(principal, rate, years):\n    return round(principal * (1 + rate) * years, 2)\n"},
     "from interest import compound\nassert compound(1000, 0.10, 2) == 1210.0, compound(1000, 0.10, 2)\nassert compound(100, 0.5, 1) == 150.0\n"),

    ("t10_median",
     "median(xs) in stats.py is wrong for even-length lists: median([1,2,3,4]) should "
     "be 2.5 (mean of the two middle values) but returns 3. Fix it.",
     {"stats.py": "def median(xs):\n    s = sorted(xs)\n    return s[len(s) // 2]\n"},
     "from stats import median\nassert median([1, 2, 3, 4]) == 2.5\nassert median([5, 1, 3]) == 3\n"),

    ("t11_fizz",
     "fizzbuzz(n) in fizz.py never returns 'FizzBuzz' for multiples of 15 because "
     "the checks are in the wrong order. Fix it.",
     {"fizz.py": "def fizzbuzz(n):\n    if n % 3 == 0:\n        return 'Fizz'\n    if n % 5 == 0:\n        return 'Buzz'\n    if n % 15 == 0:\n        return 'FizzBuzz'\n    return str(n)\n"},
     "from fizz import fizzbuzz\nassert fizzbuzz(15) == 'FizzBuzz'\nassert fizzbuzz(9) == 'Fizz'\nassert fizzbuzz(10) == 'Buzz'\nassert fizzbuzz(7) == '7'\n"),

    ("t12_cents",
     "total(prices) in till.py accumulates float error: total([0.1, 0.2]) returns "
     "0.30000000000000004. Money should be rounded to cents (2 decimals). Fix it.",
     {"till.py": "def total(prices):\n    return sum(prices)\n"},
     "from till import total\nassert total([0.1, 0.2]) == 0.3, total([0.1, 0.2])\nassert total([1.005, 2.0]) == 3.01\n"),

    ("t13_leap",
     "is_leap(year) in dates.py uses the naive %4 rule. Implement the full Gregorian "
     "rule: divisible by 4, except centuries unless divisible by 400.",
     {"dates.py": "def is_leap(year):\n    return year % 4 == 0\n"},
     "from dates import is_leap\nassert is_leap(2024) is True\nassert is_leap(1900) is False\nassert is_leap(2000) is True\nassert is_leap(2023) is False\n"),

    # ---------- string handling ----------
    ("t14_snake",
     "snake_to_title('user_name') in text.py should return 'User Name' but returns "
     "'User_Name'. Fix it.",
     {"text.py": "def snake_to_title(s):\n    return s.title()\n"},
     "from text import snake_to_title\nassert snake_to_title('user_name') == 'User Name'\nassert snake_to_title('a_b_c') == 'A B C'\n"),

    ("t15_trunc",
     "truncate(s, n) in text.py should return s unchanged when it fits, otherwise cut "
     "it so the result INCLUDING a trailing single-character ellipsis '…' is exactly n "
     "characters. truncate('hello world', 8) == 'hello w…'. Fix it.",
     {"text.py": "def truncate(s, n):\n    return s[:n]\n"},
     "from text import truncate\nassert truncate('hello world', 8) == 'hello w…', repr(truncate('hello world', 8))\nassert truncate('hi', 8) == 'hi'\n"),

    ("t16_initials",
     "initials('ada lovelace') in names.py should return 'AL' (uppercase) but returns "
     "'al'. Fix it.",
     {"names.py": "def initials(name):\n    return ''.join(w[0] for w in name.split())\n"},
     "from names import initials\nassert initials('ada lovelace') == 'AL'\nassert initials('grace') == 'G'\n"),

    ("t17_pal",
     "is_palindrome(s) in pal.py should ignore case and spaces: 'Race car' is a "
     "palindrome. Currently it does a strict comparison. Fix it.",
     {"pal.py": "def is_palindrome(s):\n    return s == s[::-1]\n"},
     "from pal import is_palindrome\nassert is_palindrome('Race car') is True\nassert is_palindrome('hello') is False\n"),

    ("t18_csv",
     "parse_line(s) in csvish.py should split on commas AND strip whitespace around "
     "each field: parse_line('a, b , c') == ['a', 'b', 'c']. Fix it.",
     {"csvish.py": "def parse_line(s):\n    return s.split(',')\n"},
     "from csvish import parse_line\nassert parse_line('a, b , c') == ['a', 'b', 'c']\nassert parse_line('x') == ['x']\n"),

    ("t19_plural",
     "pluralize(word) in words.py naively appends 's'. English words ending in "
     "s, x, ch, or sh take 'es' (box -> boxes, church -> churches). Fix it.",
     {"words.py": "def pluralize(word):\n    return word + 's'\n"},
     "from words import pluralize\nassert pluralize('box') == 'boxes'\nassert pluralize('cat') == 'cats'\nassert pluralize('church') == 'churches'\nassert pluralize('dish') == 'dishes'\n"),

    # ---------- collections / loops ----------
    ("t20_dedupe",
     "dedupe(xs) in seq.py must preserve first-seen order: dedupe([3,1,3,2,1]) == "
     "[3,1,2]. The current set() implementation loses order. Fix it.",
     {"seq.py": "def dedupe(xs):\n    return list(set(xs))\n"},
     "from seq import dedupe\nassert dedupe([3, 1, 3, 2, 1]) == [3, 1, 2]\nassert dedupe([]) == []\n"),

    ("t21_chunk",
     "chunk(xs, n) in seq.py drops the final partial chunk: chunk([1,2,3,4,5], 2) "
     "should be [[1,2],[3,4],[5]]. Fix it.",
     {"seq.py": "def chunk(xs, n):\n    return [xs[i*n:(i+1)*n] for i in range(len(xs) // n)]\n"},
     "from seq import chunk\nassert chunk([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]]\nassert chunk([1, 2], 2) == [[1, 2]]\n"),

    ("t22_flatten",
     "flatten(xs) in seq.py takes a list of lists and should return a single flat "
     "list: flatten([[1,2],[3]]) == [1,2,3]. It currently returns the input "
     "unchanged. Fix it.",
     {"seq.py": "def flatten(xs):\n    out = []\n    for x in xs:\n        out.append(x)\n    return out\n"},
     "from seq import flatten\nassert flatten([[1, 2], [3]]) == [1, 2, 3]\nassert flatten([]) == []\n"),

    ("t23_topn",
     "top_n(xs, n) in rank.py should return the n LARGEST values in descending "
     "order: top_n([5,1,9,3], 2) == [9, 5]. It returns the smallest. Fix it.",
     {"rank.py": "def top_n(xs, n):\n    return sorted(xs)[:n]\n"},
     "from rank import top_n\nassert top_n([5, 1, 9, 3], 2) == [9, 5]\nassert top_n([2], 1) == [2]\n"),

    ("t24_counts",
     "counts(text) in freq.py should count word occurrences: counts('a b a') == "
     "{'a': 2, 'b': 1}. Every count currently comes back as 1. Fix it.",
     {"freq.py": "def counts(text):\n    out = {}\n    for w in text.split():\n        out[w] = 1\n    return out\n"},
     "from freq import counts\nassert counts('a b a') == {'a': 2, 'b': 1}\nassert counts('') == {}\n"),

    ("t25_merge",
     "merge(base, override) in conf.py must let override win on key conflicts: "
     "merge({'x':1}, {'x':2,'y':3}) == {'x':2,'y':3}. It's backwards. Fix it.",
     {"conf.py": "def merge(base, override):\n    return {**override, **base}\n"},
     "from conf import merge\nassert merge({'x': 1}, {'x': 2, 'y': 3}) == {'x': 2, 'y': 3}\n"),

    # ---------- edge cases / errors ----------
    ("t26_deepget",
     "deep_get(d, path, default=None) in dicts.py reads a dotted path like 'a.b' "
     "from nested dicts. It currently raises KeyError when any key is missing; it "
     "should return the default instead.",
     {"dicts.py": "def deep_get(d, path, default=None):\n    cur = d\n    for k in path.split('.'):\n        cur = cur[k]\n    return cur\n"},
     "from dicts import deep_get\nassert deep_get({'a': {'b': 5}}, 'a.b') == 5\nassert deep_get({}, 'a.b', 0) == 0\nassert deep_get({'a': 1}, 'a.b', 'x') == 'x'\n"),

    ("t27_div",
     "safe_divide(a, b) in math2.py should return None when b is zero instead of "
     "raising ZeroDivisionError.",
     {"math2.py": "def safe_divide(a, b):\n    return a / b\n"},
     "from math2 import safe_divide\nassert safe_divide(6, 3) == 2.0\nassert safe_divide(1, 0) is None\n"),

    ("t28_parseint",
     "parse_int(s) in nums.py should return the integer for strings like ' 42 ' and "
     "None for non-numeric strings like 'abc' (it currently raises ValueError).",
     {"nums.py": "def parse_int(s):\n    return int(s)\n"},
     "from nums import parse_int\nassert parse_int(' 42 ') == 42\nassert parse_int('abc') is None\n"),

    ("t29_retry",
     "retry(f, attempts) in retry.py should call f up to `attempts` times, returning "
     "the first successful result and re-raising the last exception only if every "
     "attempt fails. It currently calls f exactly once.",
     {"retry.py": "def retry(f, attempts):\n    return f()\n"},
     "from retry import retry\ncalls = {'n': 0}\ndef flaky():\n    calls['n'] += 1\n    if calls['n'] < 3:\n        raise RuntimeError('boom')\n    return 'ok'\nassert retry(flaky, 3) == 'ok'\nassert calls['n'] == 3\ntry:\n    retry(lambda: 1 // 0, 2)\n    raise SystemExit(1)\nexcept ZeroDivisionError:\n    pass\n"),

    ("t30_age",
     "set_age(age) in person.py should raise ValueError for negative ages and return "
     "the age otherwise. It currently accepts anything.",
     {"person.py": "def set_age(age):\n    return age\n"},
     "from person import set_age\nassert set_age(5) == 5\ntry:\n    set_age(-1)\n    raise SystemExit(1)\nexcept ValueError:\n    pass\n"),

    ("t31_cfg",
     "read_config(path) in cfg.py should return {} when the file doesn't exist "
     "instead of raising FileNotFoundError. It parses JSON otherwise — keep that.",
     {"cfg.py": "import json\n\ndef read_config(path):\n    with open(path) as f:\n        return json.load(f)\n"},
     "import json, tempfile, os\nfrom cfg import read_config\nassert read_config('/nonexistent-rift-bench-xyz.json') == {}\np = tempfile.mktemp(suffix='.json')\nwith open(p, 'w') as f:\n    json.dump({'a': 1}, f)\nassert read_config(p) == {'a': 1}\nos.unlink(p)\n"),

    # ---------- implement from docstring ----------
    ("t32_slugify",
     "Implement slugify in slug.py exactly per its docstring.",
     {"slug.py": "def slugify(title):\n    \"\"\"Lower-case the title, replace runs of spaces/punctuation with single\n    hyphens, keep only [a-z0-9-], and strip leading/trailing hyphens.\n    slugify('Hello, World!') == 'hello-world'\n    \"\"\"\n    raise NotImplementedError\n"},
     "from slug import slugify\nassert slugify('Hello, World!') == 'hello-world'\nassert slugify('A  B') == 'a-b'\nassert slugify('--Already-Slug--') == 'already-slug'\n"),

    ("t33_roman",
     "Implement int_to_roman in roman.py per its docstring (1..100 is enough).",
     {"roman.py": "def int_to_roman(n):\n    \"\"\"Convert a positive integer (1..100) to a Roman numeral string.\n    4 -> 'IV', 9 -> 'IX', 14 -> 'XIV', 40 -> 'XL', 49 -> 'XLIX'.\n    \"\"\"\n    raise NotImplementedError\n"},
     "from roman import int_to_roman\nassert int_to_roman(4) == 'IV'\nassert int_to_roman(9) == 'IX'\nassert int_to_roman(14) == 'XIV'\nassert int_to_roman(40) == 'XL'\nassert int_to_roman(49) == 'XLIX'\nassert int_to_roman(100) == 'C'\n"),

    ("t34_caesar",
     "Implement caesar in cipher.py per its docstring.",
     {"cipher.py": "def caesar(s, k):\n    \"\"\"Shift letters by k positions, wrapping within the alphabet and\n    preserving case; leave non-letters unchanged.\n    caesar('abz', 1) == 'bca'; caesar('AbZ', 2) == 'CdB'.\n    \"\"\"\n    raise NotImplementedError\n"},
     "from cipher import caesar\nassert caesar('abz', 1) == 'bca'\nassert caesar('AbZ', 2) == 'CdB'\nassert caesar('a-b', 1) == 'b-c'\n"),

    ("t35_rle",
     "Implement encode in rle.py per its docstring.",
     {"rle.py": "def encode(s):\n    \"\"\"Run-length encode: each maximal run of a character becomes the\n    character followed by its count. encode('aaabb') == 'a3b2';\n    encode('abc') == 'a1b1c1'; encode('') == ''.\n    \"\"\"\n    raise NotImplementedError\n"},
     "from rle import encode\nassert encode('aaabb') == 'a3b2'\nassert encode('abc') == 'a1b1c1'\nassert encode('') == ''\n"),

    ("t36_balanced",
     "Implement balanced in brackets.py per its docstring.",
     {"brackets.py": "def balanced(s):\n    \"\"\"Return True iff every (, [, { is closed by the matching bracket in\n    the right order. balanced('([]{})') is True; balanced('(]') is False;\n    balanced('(((') is False.\n    \"\"\"\n    raise NotImplementedError\n"},
     "from brackets import balanced\nassert balanced('([]{})') is True\nassert balanced('(]') is False\nassert balanced('(((') is False\nassert balanced('') is True\n"),

    ("t37_humanize",
     "Implement humanize in dur.py per its docstring.",
     {"dur.py": "def humanize(seconds):\n    \"\"\"Format whole seconds as 'XhYmZs', omitting zero-value leading units:\n    3661 -> '1h 1m 1s', 61 -> '1m 1s', 5 -> '5s', 3600 -> '1h 0m 0s'.\n    \"\"\"\n    raise NotImplementedError\n"},
     "from dur import humanize\nassert humanize(3661) == '1h 1m 1s'\nassert humanize(61) == '1m 1s'\nassert humanize(5) == '5s'\nassert humanize(3600) == '1h 0m 0s'\n"),

    ("t38_transpose",
     "Implement transpose in matrix.py per its docstring.",
     {"matrix.py": "def transpose(m):\n    \"\"\"Transpose a non-empty rectangular matrix given as a list of rows.\n    transpose([[1,2],[3,4],[5,6]]) == [[1,3,5],[2,4,6]].\n    \"\"\"\n    raise NotImplementedError\n"},
     "from matrix import transpose\nassert transpose([[1, 2], [3, 4], [5, 6]]) == [[1, 3, 5], [2, 4, 6]]\nassert transpose([[7]]) == [[7]]\n"),

    ("t39_interleave",
     "Implement interleave in weave.py per its docstring.",
     {"weave.py": "def interleave(a, b):\n    \"\"\"Alternate elements from a and b starting with a; when one list runs\n    out, append the remainder of the other.\n    interleave([1,2,3], ['a']) == [1, 'a', 2, 3].\n    \"\"\"\n    raise NotImplementedError\n"},
     "from weave import interleave\nassert interleave([1, 2, 3], ['a']) == [1, 'a', 2, 3]\nassert interleave([], [1]) == [1]\nassert interleave([1], []) == [1]\n"),

    # ---------- multi-file bugs ----------
    ("t40_greet",
     "greet(user) in api.py prints names swapped: greet({'first':'Ada','last':'Lovelace'}) "
     "returns 'Hello, Lovelace Ada!' but should be 'Hello, Ada Lovelace!'. The bug is in "
     "the helper it uses. Fix it.",
     {"util.py": "def format_user(user):\n    return f\"{user['last']} {user['first']}\"\n",
      "api.py": "from util import format_user\n\ndef greet(user):\n    return f'Hello, {format_user(user)}!'\n"},
     "from api import greet\nassert greet({'first': 'Ada', 'last': 'Lovelace'}) == 'Hello, Ada Lovelace!'\n"),

    ("t41_tax",
     "Receipts are adding 80% tax instead of 8%. cart.total(100) should be 108.0. "
     "Find and fix the bug (it spans constants.py and cart.py).",
     {"constants.py": "# sales tax: 8%\nTAX_RATE = 0.8\n",
      "cart.py": "from constants import TAX_RATE\n\ndef total(subtotal):\n    return round(subtotal * (1 + TAX_RATE), 2)\n"},
     "from cart import total\nassert total(100) == 108.0, total(100)\nassert total(50) == 54.0\n"),

    ("t42_email",
     "forms.py imports is_email from validators.py but it was never written, so the "
     "module crashes on import. Implement a basic is_email(s): must contain exactly "
     "one '@' with a non-empty local part, and a '.' somewhere after the '@'.",
     {"validators.py": "def is_phone(s):\n    return s.replace('-', '').isdigit()\n",
      "forms.py": "from validators import is_email\n\ndef validate(address):\n    return is_email(address)\n"},
     "from forms import validate\nassert validate('a@b.co') is True\nassert validate('nope') is False\nassert validate('@b.co') is False\nassert validate('a@bco') is False\n"),

    ("t43_logger",
     "app.process('boom') should return '[error] boom' but returns '[boom] error' — "
     "somewhere the logger arguments are crossed. Fix it (files: logger.py, app.py).",
     {"logger.py": "def log(level, msg):\n    return f'[{level}] {msg}'\n",
      "app.py": "from logger import log\n\ndef process(err):\n    return log(err, 'error')\n"},
     "from app import process\nassert process('boom') == '[error] boom'\n"),

    ("t44_settings",
     "effective(user_settings) in settings.py should overlay the user's settings on "
     "top of DEFAULTS, but defaults are currently overriding the user. "
     "effective({'theme':'dark'})['theme'] must be 'dark', while unset keys fall back "
     "to defaults. Fix it.",
     {"settings.py": "DEFAULTS = {'theme': 'light', 'font': 'mono'}\n\ndef effective(user):\n    merged = dict(user)\n    merged.update(DEFAULTS)\n    return merged\n"},
     "from settings import effective\ne = effective({'theme': 'dark'})\nassert e['theme'] == 'dark'\nassert e['font'] == 'mono'\n"),

    ("t45_shapes",
     "report.total_area mixes up area and perimeter for circles: "
     "total_area([('rect',2,3),('circle',1)]) should be about 9.14 (6 + pi). Fix the "
     "bug (files: shapes.py, report.py).",
     {"shapes.py": "import math\n\ndef rect_area(w, h):\n    return w * h\n\ndef circle_area(r):\n    return math.pi * r * r\n\ndef circle_perimeter(r):\n    return 2 * math.pi * r\n",
      "report.py": "from shapes import rect_area, circle_perimeter\n\ndef total_area(items):\n    out = 0.0\n    for it in items:\n        if it[0] == 'rect':\n            out += rect_area(it[1], it[2])\n        elif it[0] == 'circle':\n            out += circle_perimeter(it[1])\n    return out\n"},
     "from report import total_area\nassert round(total_area([('rect', 2, 3), ('circle', 1)]), 2) == 9.14, total_area([('rect', 2, 3), ('circle', 1)])\n"),

    # ---------- refactors / API changes ----------
    ("t46_rename",
     "Rename the function `calc` to `calculate_total` everywhere it appears "
     "(it is defined in orders.py and used in report.py). Behavior must not change, "
     "and the old name must be gone.",
     {"orders.py": "def calc(items):\n    return sum(items)\n",
      "report.py": "import orders\n\ndef summary(items):\n    return f'total: {orders.calc(items)}'\n"},
     "import orders, report\nassert hasattr(orders, 'calculate_total')\nassert not hasattr(orders, 'calc')\nassert orders.calculate_total([2, 3]) == 5\nassert report.summary([2, 3]) == 'total: 5'\n"),

    ("t47_temps",
     "temps.py has c_to_f. Add the inverse function f_to_c(f) so that round-trips are "
     "exact for any value: f_to_c(c_to_f(25)) == 25.0 and f_to_c(32) == 0.0.",
     {"temps.py": "def c_to_f(c):\n    return c * 9 / 5 + 32\n"},
     "from temps import c_to_f, f_to_c\nassert f_to_c(c_to_f(25)) == 25.0\nassert f_to_c(32) == 0.0\nassert f_to_c(212) == 100.0\n"),

    ("t48_storage",
     "Extend storage.py: save(key, value) must return True on success, and add "
     "load(key, default=None) returning the saved value or the default. Keep the "
     "module-level dict approach.",
     {"storage.py": "_DB = {}\n\ndef save(key, value):\n    _DB[key] = value\n"},
     "import storage\nassert storage.save('a', 1) is True\nassert storage.load('a') == 1\nassert storage.load('missing', 42) == 42\nassert storage.load('missing') is None\n"),

    ("t49_user",
     "main.py crashes because User requires an email. Make email optional with "
     "default '' (keeping name required) so both constructions work.",
     {"user.py": "class User:\n    def __init__(self, name, email):\n        self.name = name\n        self.email = email\n",
      "main.py": "from user import User\n\ndef make_users():\n    return [User('ada', 'ada@x.io'), User('grace')]\n"},
     "from main import make_users\nus = make_users()\nassert us[0].email == 'ada@x.io'\nassert us[1].email == ''\nassert us[1].name == 'grace'\n"),

    ("t50_cli",
     "parse_args(argv) in cli.py should support '--name VALUE' (string, default None) "
     "and '--loud' (boolean flag, default False), returning "
     "{'name': ..., 'loud': ...}. The current version is broken: the flag eats the "
     "next argument and defaults are missing. Fix it.",
     {"cli.py": "def parse_args(argv):\n    out = {}\n    i = 0\n    while i < len(argv):\n        key = argv[i].lstrip('-')\n        out[key] = argv[i + 1]\n        i += 2\n    return out\n"},
     "from cli import parse_args\nassert parse_args(['--name', 'ada', '--loud']) == {'name': 'ada', 'loud': True}\nassert parse_args([]) == {'name': None, 'loud': False}\nassert parse_args(['--loud', '--name', 'x']) == {'name': 'x', 'loud': True}\n"),
]

VERIFY_TEMPLATE = """#!/usr/bin/env bash
set -e
python3 - <<'PYEOF'
{body}print('VERIFY OK')
PYEOF
"""


def main():
    assert len(TASKS) == 45, len(TASKS)
    names = [t[0] for t in TASKS]
    assert len(set(names)) == 45
    for name, prompt, files, verify_body in TASKS:
        d = os.path.join(ROOT, name)
        os.makedirs(d, exist_ok=True)
        for fname, content in files.items():
            with open(os.path.join(d, fname), "w") as f:
                f.write(content)
        with open(os.path.join(d, "prompt.txt"), "w") as f:
            f.write(prompt + "\n")
        vpath = os.path.join(d, "verify.sh")
        with open(vpath, "w") as f:
            f.write(VERIFY_TEMPLATE.format(body=verify_body))
        os.chmod(vpath, os.stat(vpath).st_mode | stat.S_IEXEC)

    # Self-check: every verify must FAIL on the broken fixture.
    bad = []
    for name, _, _, _ in TASKS:
        d = os.path.join(ROOT, name)
        r = subprocess.run(["bash", "verify.sh"], cwd=d, capture_output=True)
        if r.returncode == 0:
            bad.append(name)
    if bad:
        print(f"SELF-CHECK FAILED — these verifies pass on broken code: {bad}")
        sys.exit(1)
    print(f"generated {len(TASKS)} tasks; all verifies correctly fail pre-fix")


if __name__ == "__main__":
    main()
