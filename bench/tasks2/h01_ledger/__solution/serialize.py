"""JSON-dict (de)serialisation for transactions."""
from models import Transaction


def tx_to_dict(tx):
    """Serialise a Transaction to a plain dict."""
    d = {
        "txid": tx.txid,
        "date": tx.date,
        "kind": tx.kind,
        "amount_cents": tx.amount_cents,
        "account": tx.account,
    }
    if tx.target is not None:
        d["target"] = tx.target
    return d


def tx_from_dict(d):
    """Build a Transaction from a plain dict."""
    return Transaction(
        txid=d["txid"],
        date=d["date"],
        kind=d["kind"],
        amount_cents=int(d["amount_cents"]),
        account=d["account"],
        target=d.get("target"),
    )
