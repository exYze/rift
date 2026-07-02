"""Data model for ledger transactions."""
from dataclasses import dataclass
from typing import Optional


@dataclass
class Transaction:
    """One ledger entry.

    kind is one of "deposit", "withdrawal" or "transfer".  For a
    transfer, `target` names the account that receives `amount_cents`
    while `account` is debited.
    """
    txid: str
    date: str  # YYYY-MM-DD
    kind: str
    amount_cents: int
    account: str
    target: Optional[str] = None
