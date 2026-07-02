"""The ledger: applies validated transactions to account balances."""


class Ledger:
    def __init__(self):
        self.balances = {}

    def apply(self, tx):
        """Apply one transaction to the balances."""
        if tx.kind == "deposit":
            self._credit(tx.account, tx.amount_cents)
        elif tx.kind == "withdrawal":
            self._debit(tx.account, tx.amount_cents)
        else:
            raise ValueError(f"{tx.txid}: cannot apply kind {tx.kind!r}")

    def _credit(self, account, cents):
        self.balances[account] = self.balances.get(account, 0) + cents

    def _debit(self, account, cents):
        self.balances[account] = self.balances.get(account, 0) - cents

    def balance(self, account):
        return self.balances.get(account, 0)

    def accounts(self):
        return sorted(self.balances)
