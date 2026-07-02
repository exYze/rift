"""Print end-of-month balances for the March export.

Usage: python3 main.py
Prints one "account cents" line per account, sorted by account name.
"""
import json

from serialize import tx_from_dict
from validate import validate
from ledger import Ledger

MARCH_EXPORT = """
[
  {"txid": "t1", "date": "2026-03-01", "kind": "deposit",
   "amount_cents": 10000, "account": "checking"},
  {"txid": "t2", "date": "2026-03-02", "kind": "deposit",
   "amount_cents": 5000, "account": "savings"},
  {"txid": "t3", "date": "2026-03-05", "kind": "transfer",
   "amount_cents": 2500, "account": "checking", "target": "savings"},
  {"txid": "t4", "date": "2026-03-09", "kind": "withdrawal",
   "amount_cents": 1500, "account": "checking"},
  {"txid": "t5", "date": "2026-03-21", "kind": "transfer",
   "amount_cents": 100, "account": "savings", "target": "vault"}
]
"""


def main():
    ledger = Ledger()
    for raw in json.loads(MARCH_EXPORT):
        tx = validate(tx_from_dict(raw))
        ledger.apply(tx)
    for account in ledger.accounts():
        print(account, ledger.balance(account))


if __name__ == "__main__":
    main()
