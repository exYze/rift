"""Validation rules for transactions before they hit the ledger."""

KINDS = {"deposit", "withdrawal", "transfer"}


class ValidationError(ValueError):
    pass


def validate(tx):
    """Check a transaction; return it unchanged or raise ValidationError."""
    if tx.kind not in KINDS:
        raise ValidationError(f"{tx.txid}: unknown kind {tx.kind!r}")
    if tx.amount_cents <= 0:
        raise ValidationError(f"{tx.txid}: amount must be positive")
    if not tx.account:
        raise ValidationError(f"{tx.txid}: missing account")
    if tx.kind == "transfer":
        if not tx.target:
            raise ValidationError(f"{tx.txid}: transfer needs a target")
        if tx.target == tx.account:
            raise ValidationError(f"{tx.txid}: transfer to same account")
    return tx
