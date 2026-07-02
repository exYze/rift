#!/usr/bin/env bash
set -e
out="$(python3 main.py)"
expected="checking 6000
savings 7400
vault 100"
[ "$out" = "$expected" ]
python3 - <<'PYEOF'
from models import Transaction
from serialize import tx_to_dict, tx_from_dict
from validate import validate
from ledger import Ledger

# Serializer round-trips the transfer target.
t = Transaction(txid="x1", date="2026-03-01", kind="transfer",
                amount_cents=300, account="a", target="b")
rt = tx_from_dict(tx_to_dict(t))
assert rt.target == "b", rt
assert rt.amount_cents == 300 and rt.kind == "transfer"

# Deposits/withdrawals unchanged by the round trip.
d = Transaction(txid="x2", date="2026-03-02", kind="deposit",
                amount_cents=100, account="a")
assert tx_from_dict(tx_to_dict(d)).target is None

# Validator accepts a good transfer, rejects bad ones.
validate(t)
for bad in (
    Transaction(txid="b1", date="2026-03-01", kind="transfer",
                amount_cents=100, account="a"),           # no target
    Transaction(txid="b2", date="2026-03-01", kind="transfer",
                amount_cents=100, account="a", target=""),  # empty target
    Transaction(txid="b3", date="2026-03-01", kind="transfer",
                amount_cents=100, account="a", target="a"),  # self transfer
    Transaction(txid="b4", date="2026-03-01", kind="transfer",
                amount_cents=-5, account="a", target="b"),   # bad amount
    Transaction(txid="b5", date="2026-03-01", kind="mystery",
                amount_cents=100, account="a"),              # unknown kind
):
    try:
        validate(bad)
        raise SystemExit(f"validator accepted {bad.txid}")
    except SystemExit:
        raise
    except Exception:
        pass

# Ledger moves money on transfer.
led = Ledger()
led.apply(Transaction(txid="y1", date="2026-03-01", kind="deposit",
                      amount_cents=1000, account="a"))
led.apply(Transaction(txid="y2", date="2026-03-02", kind="transfer",
                      amount_cents=400, account="a", target="b"))
assert led.balance("a") == 600, led.balances
assert led.balance("b") == 400, led.balances
led.apply(Transaction(txid="y3", date="2026-03-03", kind="withdrawal",
                      amount_cents=100, account="b"))
assert led.balance("b") == 300, led.balances
print("VERIFY OK")
PYEOF
