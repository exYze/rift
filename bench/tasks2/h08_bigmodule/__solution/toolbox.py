"""toolbox: grab-bag of small utilities used by the report generator.

String helpers, event handlers, rollups, validators, formatters and
collection utilities. Everything here is dependency-free stdlib Python.
"""
import math
import re


# ---------------------------------------------------------------------------
# String helpers
# ---------------------------------------------------------------------------

def to_snake(name):
    """Convert CamelCase or kebab-case to snake_case."""
    name = name.replace("-", "_")
    out = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name)
    return out.lower()


def to_kebab(name):
    """Convert CamelCase or snake_case to kebab-case."""
    return to_snake(name).replace("_", "-")


def to_camel(name):
    """Convert snake_case or kebab-case to camelCase."""
    parts = re.split(r"[-_]+", name.strip("-_"))
    if not parts or not parts[0]:
        return ""
    return parts[0].lower() + "".join(p.title() for p in parts[1:])


def to_pascal(name):
    """Convert snake_case or kebab-case to PascalCase."""
    return "".join(p.title() for p in re.split(r"[-_]+", name) if p)


def squeeze_ws(text):
    """Collapse runs of whitespace into single spaces and strip the ends."""
    return re.sub(r"\s+", " ", text).strip()


def truncate_end(text, limit, ellipsis="..."):
    """Cut `text` to at most `limit` chars, ending in `ellipsis` if cut."""
    if len(text) <= limit:
        return text
    if limit <= len(ellipsis):
        return ellipsis[:limit]
    return text[: limit - len(ellipsis)] + ellipsis


def truncate_middle(text, limit, ellipsis="..."):
    """Cut the middle out of `text` so it fits in `limit` chars."""
    if len(text) <= limit:
        return text
    if limit <= len(ellipsis):
        return ellipsis[:limit]
    keep = limit - len(ellipsis)
    head = (keep + 1) // 2
    tail = keep - head
    return text[:head] + ellipsis + (text[-tail:] if tail else "")


def pad_left(text, width, fill=" "):
    """Left-pad `text` with `fill` to `width` characters."""
    return text.rjust(width, fill)


def pad_right(text, width, fill=" "):
    """Right-pad `text` with `fill` to `width` characters."""
    return text.ljust(width, fill)


def pad_center(text, width, fill=" "):
    """Center `text` in a field of `width` characters."""
    return text.center(width, fill)


def indent_lines(text, prefix="    "):
    """Prefix every non-empty line of `text`."""
    return "\n".join(prefix + ln if ln else ln for ln in text.split("\n"))


def dedent_common(text):
    """Strip the largest common leading run of spaces from all lines."""
    lines = [ln for ln in text.split("\n") if ln.strip()]
    if not lines:
        return text
    margin = min(len(ln) - len(ln.lstrip(" ")) for ln in lines)
    return "\n".join(ln[margin:] if ln.strip() else ln for ln in text.split("\n"))


def strip_prefix(text, prefix):
    """Remove `prefix` from the start of `text` when present."""
    return text[len(prefix):] if text.startswith(prefix) else text


def strip_suffix(text, suffix):
    """Remove `suffix` from the end of `text` when present."""
    return text[: -len(suffix)] if suffix and text.endswith(suffix) else text


def surround(text, wrapper):
    """Wrap `text` in `wrapper` on both sides."""
    return f"{wrapper}{text}{wrapper}"


def initials(name):
    """First letter of every word, uppercased: "ada lovelace" -> "AL"."""
    return "".join(w[0].upper() for w in name.split() if w)


def count_words(text):
    """Number of whitespace-separated words."""
    return len(text.split())


def is_blank(text):
    """True when `text` is empty or only whitespace."""
    return not text or text.isspace()


def ensure_trailing(text, char):
    """Append `char` unless `text` already ends with it."""
    return text if text.endswith(char) else text + char


def swap_case_words(text):
    """Swap the case of every alphabetic character."""
    return text.swapcase()

# ---------------------------------------------------------------------------
# Event handlers: core entities
# ---------------------------------------------------------------------------


def handle_user_created(record, state):
    """Handle a 'user.created' event.

    Bumps the running 'created' counter for users in `state` and remembers
    the id of the last user that was created. Returns the new count.
    """
    bucket = state.setdefault("user", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_user_updated(record, state):
    """Handle a 'user.updated' event.

    Bumps the running 'updated' counter for users in `state` and remembers
    the id of the last user that was updated. Returns the new count.
    """
    bucket = state.setdefault("user", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_user_deleted(record, state):
    """Handle a 'user.deleted' event.

    Bumps the running 'deleted' counter for users in `state` and remembers
    the id of the last user that was deleted. Returns the new count.
    """
    bucket = state.setdefault("user", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_user_archived(record, state):
    """Handle a 'user.archived' event.

    Bumps the running 'archived' counter for users in `state` and remembers
    the id of the last user that was archived. Returns the new count.
    """
    bucket = state.setdefault("user", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_user_restored(record, state):
    """Handle a 'user.restored' event.

    Bumps the running 'restored' counter for users in `state` and remembers
    the id of the last user that was restored. Returns the new count.
    """
    bucket = state.setdefault("user", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_order_created(record, state):
    """Handle a 'order.created' event.

    Bumps the running 'created' counter for orders in `state` and remembers
    the id of the last order that was created. Returns the new count.
    """
    bucket = state.setdefault("order", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_order_updated(record, state):
    """Handle a 'order.updated' event.

    Bumps the running 'updated' counter for orders in `state` and remembers
    the id of the last order that was updated. Returns the new count.
    """
    bucket = state.setdefault("order", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_order_deleted(record, state):
    """Handle a 'order.deleted' event.

    Bumps the running 'deleted' counter for orders in `state` and remembers
    the id of the last order that was deleted. Returns the new count.
    """
    bucket = state.setdefault("order", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_order_archived(record, state):
    """Handle a 'order.archived' event.

    Bumps the running 'archived' counter for orders in `state` and remembers
    the id of the last order that was archived. Returns the new count.
    """
    bucket = state.setdefault("order", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_order_restored(record, state):
    """Handle a 'order.restored' event.

    Bumps the running 'restored' counter for orders in `state` and remembers
    the id of the last order that was restored. Returns the new count.
    """
    bucket = state.setdefault("order", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_invoice_created(record, state):
    """Handle a 'invoice.created' event.

    Bumps the running 'created' counter for invoices in `state` and remembers
    the id of the last invoice that was created. Returns the new count.
    """
    bucket = state.setdefault("invoice", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_invoice_updated(record, state):
    """Handle a 'invoice.updated' event.

    Bumps the running 'updated' counter for invoices in `state` and remembers
    the id of the last invoice that was updated. Returns the new count.
    """
    bucket = state.setdefault("invoice", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_invoice_deleted(record, state):
    """Handle a 'invoice.deleted' event.

    Bumps the running 'deleted' counter for invoices in `state` and remembers
    the id of the last invoice that was deleted. Returns the new count.
    """
    bucket = state.setdefault("invoice", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_invoice_archived(record, state):
    """Handle a 'invoice.archived' event.

    Bumps the running 'archived' counter for invoices in `state` and remembers
    the id of the last invoice that was archived. Returns the new count.
    """
    bucket = state.setdefault("invoice", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_invoice_restored(record, state):
    """Handle a 'invoice.restored' event.

    Bumps the running 'restored' counter for invoices in `state` and remembers
    the id of the last invoice that was restored. Returns the new count.
    """
    bucket = state.setdefault("invoice", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_ticket_created(record, state):
    """Handle a 'ticket.created' event.

    Bumps the running 'created' counter for tickets in `state` and remembers
    the id of the last ticket that was created. Returns the new count.
    """
    bucket = state.setdefault("ticket", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_ticket_updated(record, state):
    """Handle a 'ticket.updated' event.

    Bumps the running 'updated' counter for tickets in `state` and remembers
    the id of the last ticket that was updated. Returns the new count.
    """
    bucket = state.setdefault("ticket", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_ticket_deleted(record, state):
    """Handle a 'ticket.deleted' event.

    Bumps the running 'deleted' counter for tickets in `state` and remembers
    the id of the last ticket that was deleted. Returns the new count.
    """
    bucket = state.setdefault("ticket", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_ticket_archived(record, state):
    """Handle a 'ticket.archived' event.

    Bumps the running 'archived' counter for tickets in `state` and remembers
    the id of the last ticket that was archived. Returns the new count.
    """
    bucket = state.setdefault("ticket", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_ticket_restored(record, state):
    """Handle a 'ticket.restored' event.

    Bumps the running 'restored' counter for tickets in `state` and remembers
    the id of the last ticket that was restored. Returns the new count.
    """
    bucket = state.setdefault("ticket", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_session_created(record, state):
    """Handle a 'session.created' event.

    Bumps the running 'created' counter for sessions in `state` and remembers
    the id of the last session that was created. Returns the new count.
    """
    bucket = state.setdefault("session", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_session_updated(record, state):
    """Handle a 'session.updated' event.

    Bumps the running 'updated' counter for sessions in `state` and remembers
    the id of the last session that was updated. Returns the new count.
    """
    bucket = state.setdefault("session", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_session_deleted(record, state):
    """Handle a 'session.deleted' event.

    Bumps the running 'deleted' counter for sessions in `state` and remembers
    the id of the last session that was deleted. Returns the new count.
    """
    bucket = state.setdefault("session", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_session_archived(record, state):
    """Handle a 'session.archived' event.

    Bumps the running 'archived' counter for sessions in `state` and remembers
    the id of the last session that was archived. Returns the new count.
    """
    bucket = state.setdefault("session", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_session_restored(record, state):
    """Handle a 'session.restored' event.

    Bumps the running 'restored' counter for sessions in `state` and remembers
    the id of the last session that was restored. Returns the new count.
    """
    bucket = state.setdefault("session", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_payment_created(record, state):
    """Handle a 'payment.created' event.

    Bumps the running 'created' counter for payments in `state` and remembers
    the id of the last payment that was created. Returns the new count.
    """
    bucket = state.setdefault("payment", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_payment_updated(record, state):
    """Handle a 'payment.updated' event.

    Bumps the running 'updated' counter for payments in `state` and remembers
    the id of the last payment that was updated. Returns the new count.
    """
    bucket = state.setdefault("payment", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_payment_deleted(record, state):
    """Handle a 'payment.deleted' event.

    Bumps the running 'deleted' counter for payments in `state` and remembers
    the id of the last payment that was deleted. Returns the new count.
    """
    bucket = state.setdefault("payment", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_payment_archived(record, state):
    """Handle a 'payment.archived' event.

    Bumps the running 'archived' counter for payments in `state` and remembers
    the id of the last payment that was archived. Returns the new count.
    """
    bucket = state.setdefault("payment", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_payment_restored(record, state):
    """Handle a 'payment.restored' event.

    Bumps the running 'restored' counter for payments in `state` and remembers
    the id of the last payment that was restored. Returns the new count.
    """
    bucket = state.setdefault("payment", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_subscription_created(record, state):
    """Handle a 'subscription.created' event.

    Bumps the running 'created' counter for subscriptions in `state` and remembers
    the id of the last subscription that was created. Returns the new count.
    """
    bucket = state.setdefault("subscription", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_subscription_updated(record, state):
    """Handle a 'subscription.updated' event.

    Bumps the running 'updated' counter for subscriptions in `state` and remembers
    the id of the last subscription that was updated. Returns the new count.
    """
    bucket = state.setdefault("subscription", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_subscription_deleted(record, state):
    """Handle a 'subscription.deleted' event.

    Bumps the running 'deleted' counter for subscriptions in `state` and remembers
    the id of the last subscription that was deleted. Returns the new count.
    """
    bucket = state.setdefault("subscription", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_subscription_archived(record, state):
    """Handle a 'subscription.archived' event.

    Bumps the running 'archived' counter for subscriptions in `state` and remembers
    the id of the last subscription that was archived. Returns the new count.
    """
    bucket = state.setdefault("subscription", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_subscription_restored(record, state):
    """Handle a 'subscription.restored' event.

    Bumps the running 'restored' counter for subscriptions in `state` and remembers
    the id of the last subscription that was restored. Returns the new count.
    """
    bucket = state.setdefault("subscription", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count

# ---------------------------------------------------------------------------
# Row rollups: aggregate one column across a list of dict rows
# ---------------------------------------------------------------------------


def rollup_sum(rows, key):
    """Sum of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns 0.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return 0
    return sum(values)


def rollup_min(rows, key):
    """Minimum of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns None.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return None
    return min(values)


def rollup_max(rows, key):
    """Maximum of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns None.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return None
    return max(values)


def rollup_count(rows, key):
    """Count of present values of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns 0.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return 0
    return len(values)


def rollup_first(rows, key):
    """First present value of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns None.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return None
    return values[0]


def rollup_last(rows, key):
    """Last present value of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns None.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return None
    return values[-1]


def rollup_any(rows, key):
    """True when any value is truthy of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns False.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return False
    return any(values)


def rollup_all(rows, key):
    """True when every value is truthy of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns True.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return True
    return all(values)


def rollup_uniq(rows, key):
    """Sorted unique values of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns [].
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return []
    return sorted(set(values))


def rollup_span(rows, key):
    """max - min of the values of the `key` column across dict rows.

    Rows without the key are skipped; with no values present this
    returns None.
    """
    values = [row[key] for row in rows if key in row]
    if not values:
        return None
    return max(values) - min(values)

# ---------------------------------------------------------------------------
# Shape validators
# ---------------------------------------------------------------------------


_SLUG_RE = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")


def is_slug(text):
    """True when `text` looks like a URL slug like 'big-report-3'."""
    return isinstance(text, str) and bool(_SLUG_RE.fullmatch(text))


_HEX_COLOR_RE = re.compile(r"#[0-9a-fA-F]{6}")


def is_hex_color(text):
    """True when `text` looks like a CSS hex colour like '#ff8800'."""
    return isinstance(text, str) and bool(_HEX_COLOR_RE.fullmatch(text))


_UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def is_uuid(text):
    """True when `text` looks like a lowercase UUID."""
    return isinstance(text, str) and bool(_UUID_RE.fullmatch(text))


_IPV4_RE = re.compile(r"(?:\d{1,3}\.){3}\d{1,3}")


def is_ipv4(text):
    """True when `text` looks like a dotted-quad IPv4 address (loose)."""
    return isinstance(text, str) and bool(_IPV4_RE.fullmatch(text))


_ISO_DATE_RE = re.compile(r"\d{4}-\d{2}-\d{2}")


def is_iso_date(text):
    """True when `text` looks like an ISO date like 2026-03-01 (shape only)."""
    return isinstance(text, str) and bool(_ISO_DATE_RE.fullmatch(text))


_SEMVER_RE = re.compile(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?")


def is_semver(text):
    """True when `text` looks like a semantic version."""
    return isinstance(text, str) and bool(_SEMVER_RE.fullmatch(text))


_USERNAME_RE = re.compile(r"[a-zA-Z][a-zA-Z0-9_]{2,31}")


def is_username(text):
    """True when `text` looks like a login name."""
    return isinstance(text, str) and bool(_USERNAME_RE.fullmatch(text))


_HOSTNAME_RE = re.compile(r"[a-z0-9]+(?:[.-][a-z0-9]+)*")


def is_hostname(text):
    """True when `text` looks like a lowercase hostname (loose)."""
    return isinstance(text, str) and bool(_HOSTNAME_RE.fullmatch(text))


_PORT_RE = re.compile(r"[0-9]{1,5}")


def is_port(text):
    """True when `text` looks like a TCP port number (shape only)."""
    return isinstance(text, str) and bool(_PORT_RE.fullmatch(text))


_SHA1_RE = re.compile(r"[0-9a-f]{40}")


def is_sha1(text):
    """True when `text` looks like a lowercase SHA-1 hex digest."""
    return isinstance(text, str) and bool(_SHA1_RE.fullmatch(text))


_ISO_TIME_RE = re.compile(r"\d{2}:\d{2}(?::\d{2})?")


def is_iso_time(text):
    """True when `text` looks like a clock time like 14:30 or 14:30:59."""
    return isinstance(text, str) and bool(_ISO_TIME_RE.fullmatch(text))


_EMAIL_LOOSE_RE = re.compile(r"[^@\s]+@[^@\s]+\.[^@\s]+")


def is_email_loose(text):
    """True when `text` looks like an email address (loose shape)."""
    return isinstance(text, str) and bool(_EMAIL_LOOSE_RE.fullmatch(text))

# ---------------------------------------------------------------------------
# Formatting helpers
# ---------------------------------------------------------------------------

_BINARY_UNITS = ("B", "KB", "MB", "GB", "TB", "PB")
_SI_UNITS = ("B", "kB", "MB", "GB", "TB", "PB")


def format_size(n):
    """Format a byte count with binary (1024-based) units.

    Sizes under one kilobyte print as a bare integer with " B"; larger
    sizes use one decimal place, e.g. 1536 -> "1.5 KB".
    """
    if n < 0:
        raise ValueError("size must be non-negative")
    value = float(n)
    idx = 0
    while value >= 1024.0 and idx < len(_BINARY_UNITS) - 1:
        value /= 1024.0
        idx += 1
    if idx == 0:
        return "%d %s" % (int(value), _BINARY_UNITS[0])
    return "%.1f %s" % (value, _BINARY_UNITS[idx])


def format_size_si(n):
    """Format a byte count with SI (1000-based) units.

    Same shape as format_size(): "500 B", "1.0 kB", "2.5 MB".
    """
    if n < 0:
        raise ValueError("size must be non-negative")
    value = float(n)
    idx = 0
    while value >= 1000.0 and idx < len(_SI_UNITS) - 1:
        value /= 1000.0
        idx += 1
    if idx == 0:
        return "%d %s" % (int(value), _SI_UNITS[0])
    return "%.1f %s" % (value, _SI_UNITS[idx])


def scale_bytes(n):
    """Return (value, unit) for a byte count in binary units.

    Unlike format_size() this returns the raw scaled float so callers
    can pick their own precision.
    """
    if n < 0:
        raise ValueError("size must be non-negative")
    value = float(n)
    idx = 0
    while value >= 1024.0 and idx < len(_BINARY_UNITS) - 1:
        value /= 1024.0
        idx += 1
    return value, _BINARY_UNITS[idx]


def format_rate(n_per_sec):
    """Format a byte throughput, e.g. 2048 -> "2.0 KB/s"."""
    return format_size(n_per_sec) + "/s"


def format_count(n):
    """Format a plain count with thousands separators: 1234567 -> "1,234,567"."""
    return f"{n:,}"


def format_pct(part, whole):
    """Percentage string with one decimal, "0.0%" when whole is zero."""
    if whole == 0:
        return "0.0%"
    return "%.1f%%" % (100.0 * part / whole)


def format_signed(n):
    """Integer with an explicit sign: +3 / -2 / +0."""
    return f"+{n}" if n >= 0 else str(n)


def format_ms(ms):
    """Milliseconds as a compact human string: 1500 -> "1.5s", 90000 -> "1m30s"."""
    if ms < 0:
        raise ValueError("negative duration")
    if ms < 1000:
        return f"{ms}ms"
    seconds, ms = divmod(ms, 1000)
    if seconds < 60:
        return f"{seconds}.{ms // 100}s" if ms else f"{seconds}s"
    minutes, seconds = divmod(seconds, 60)
    return f"{minutes}m{seconds}s" if seconds else f"{minutes}m"


def format_row(cells, widths):
    """Left-align each cell in its column width, joined by two spaces."""
    return "  ".join(str(c).ljust(w) for c, w in zip(cells, widths))

# ---------------------------------------------------------------------------
# Event handlers: fulfilment and platform entities
# ---------------------------------------------------------------------------


def handle_shipment_created(record, state):
    """Handle a 'shipment.created' event.

    Bumps the running 'created' counter for shipments in `state` and remembers
    the id of the last shipment that was created. Returns the new count.
    """
    bucket = state.setdefault("shipment", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_shipment_updated(record, state):
    """Handle a 'shipment.updated' event.

    Bumps the running 'updated' counter for shipments in `state` and remembers
    the id of the last shipment that was updated. Returns the new count.
    """
    bucket = state.setdefault("shipment", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_shipment_deleted(record, state):
    """Handle a 'shipment.deleted' event.

    Bumps the running 'deleted' counter for shipments in `state` and remembers
    the id of the last shipment that was deleted. Returns the new count.
    """
    bucket = state.setdefault("shipment", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_shipment_archived(record, state):
    """Handle a 'shipment.archived' event.

    Bumps the running 'archived' counter for shipments in `state` and remembers
    the id of the last shipment that was archived. Returns the new count.
    """
    bucket = state.setdefault("shipment", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_shipment_restored(record, state):
    """Handle a 'shipment.restored' event.

    Bumps the running 'restored' counter for shipments in `state` and remembers
    the id of the last shipment that was restored. Returns the new count.
    """
    bucket = state.setdefault("shipment", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_account_created(record, state):
    """Handle a 'account.created' event.

    Bumps the running 'created' counter for accounts in `state` and remembers
    the id of the last account that was created. Returns the new count.
    """
    bucket = state.setdefault("account", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_account_updated(record, state):
    """Handle a 'account.updated' event.

    Bumps the running 'updated' counter for accounts in `state` and remembers
    the id of the last account that was updated. Returns the new count.
    """
    bucket = state.setdefault("account", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_account_deleted(record, state):
    """Handle a 'account.deleted' event.

    Bumps the running 'deleted' counter for accounts in `state` and remembers
    the id of the last account that was deleted. Returns the new count.
    """
    bucket = state.setdefault("account", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_account_archived(record, state):
    """Handle a 'account.archived' event.

    Bumps the running 'archived' counter for accounts in `state` and remembers
    the id of the last account that was archived. Returns the new count.
    """
    bucket = state.setdefault("account", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_account_restored(record, state):
    """Handle a 'account.restored' event.

    Bumps the running 'restored' counter for accounts in `state` and remembers
    the id of the last account that was restored. Returns the new count.
    """
    bucket = state.setdefault("account", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_report_created(record, state):
    """Handle a 'report.created' event.

    Bumps the running 'created' counter for reports in `state` and remembers
    the id of the last report that was created. Returns the new count.
    """
    bucket = state.setdefault("report", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_report_updated(record, state):
    """Handle a 'report.updated' event.

    Bumps the running 'updated' counter for reports in `state` and remembers
    the id of the last report that was updated. Returns the new count.
    """
    bucket = state.setdefault("report", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_report_deleted(record, state):
    """Handle a 'report.deleted' event.

    Bumps the running 'deleted' counter for reports in `state` and remembers
    the id of the last report that was deleted. Returns the new count.
    """
    bucket = state.setdefault("report", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_report_archived(record, state):
    """Handle a 'report.archived' event.

    Bumps the running 'archived' counter for reports in `state` and remembers
    the id of the last report that was archived. Returns the new count.
    """
    bucket = state.setdefault("report", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_report_restored(record, state):
    """Handle a 'report.restored' event.

    Bumps the running 'restored' counter for reports in `state` and remembers
    the id of the last report that was restored. Returns the new count.
    """
    bucket = state.setdefault("report", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_webhook_created(record, state):
    """Handle a 'webhook.created' event.

    Bumps the running 'created' counter for webhooks in `state` and remembers
    the id of the last webhook that was created. Returns the new count.
    """
    bucket = state.setdefault("webhook", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_webhook_updated(record, state):
    """Handle a 'webhook.updated' event.

    Bumps the running 'updated' counter for webhooks in `state` and remembers
    the id of the last webhook that was updated. Returns the new count.
    """
    bucket = state.setdefault("webhook", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_webhook_deleted(record, state):
    """Handle a 'webhook.deleted' event.

    Bumps the running 'deleted' counter for webhooks in `state` and remembers
    the id of the last webhook that was deleted. Returns the new count.
    """
    bucket = state.setdefault("webhook", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_webhook_archived(record, state):
    """Handle a 'webhook.archived' event.

    Bumps the running 'archived' counter for webhooks in `state` and remembers
    the id of the last webhook that was archived. Returns the new count.
    """
    bucket = state.setdefault("webhook", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_webhook_restored(record, state):
    """Handle a 'webhook.restored' event.

    Bumps the running 'restored' counter for webhooks in `state` and remembers
    the id of the last webhook that was restored. Returns the new count.
    """
    bucket = state.setdefault("webhook", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_device_created(record, state):
    """Handle a 'device.created' event.

    Bumps the running 'created' counter for devices in `state` and remembers
    the id of the last device that was created. Returns the new count.
    """
    bucket = state.setdefault("device", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_device_updated(record, state):
    """Handle a 'device.updated' event.

    Bumps the running 'updated' counter for devices in `state` and remembers
    the id of the last device that was updated. Returns the new count.
    """
    bucket = state.setdefault("device", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_device_deleted(record, state):
    """Handle a 'device.deleted' event.

    Bumps the running 'deleted' counter for devices in `state` and remembers
    the id of the last device that was deleted. Returns the new count.
    """
    bucket = state.setdefault("device", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_device_archived(record, state):
    """Handle a 'device.archived' event.

    Bumps the running 'archived' counter for devices in `state` and remembers
    the id of the last device that was archived. Returns the new count.
    """
    bucket = state.setdefault("device", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_device_restored(record, state):
    """Handle a 'device.restored' event.

    Bumps the running 'restored' counter for devices in `state` and remembers
    the id of the last device that was restored. Returns the new count.
    """
    bucket = state.setdefault("device", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_license_created(record, state):
    """Handle a 'license.created' event.

    Bumps the running 'created' counter for licenses in `state` and remembers
    the id of the last license that was created. Returns the new count.
    """
    bucket = state.setdefault("license", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_license_updated(record, state):
    """Handle a 'license.updated' event.

    Bumps the running 'updated' counter for licenses in `state` and remembers
    the id of the last license that was updated. Returns the new count.
    """
    bucket = state.setdefault("license", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_license_deleted(record, state):
    """Handle a 'license.deleted' event.

    Bumps the running 'deleted' counter for licenses in `state` and remembers
    the id of the last license that was deleted. Returns the new count.
    """
    bucket = state.setdefault("license", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_license_archived(record, state):
    """Handle a 'license.archived' event.

    Bumps the running 'archived' counter for licenses in `state` and remembers
    the id of the last license that was archived. Returns the new count.
    """
    bucket = state.setdefault("license", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_license_restored(record, state):
    """Handle a 'license.restored' event.

    Bumps the running 'restored' counter for licenses in `state` and remembers
    the id of the last license that was restored. Returns the new count.
    """
    bucket = state.setdefault("license", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count


def handle_warehouse_created(record, state):
    """Handle a 'warehouse.created' event.

    Bumps the running 'created' counter for warehouses in `state` and remembers
    the id of the last warehouse that was created. Returns the new count.
    """
    bucket = state.setdefault("warehouse", {})
    count = bucket.get("created", 0) + 1
    bucket["created"] = count
    if "id" in record:
        bucket["last_created_id"] = record["id"]
    return count


def handle_warehouse_updated(record, state):
    """Handle a 'warehouse.updated' event.

    Bumps the running 'updated' counter for warehouses in `state` and remembers
    the id of the last warehouse that was updated. Returns the new count.
    """
    bucket = state.setdefault("warehouse", {})
    count = bucket.get("updated", 0) + 1
    bucket["updated"] = count
    if "id" in record:
        bucket["last_updated_id"] = record["id"]
    return count


def handle_warehouse_deleted(record, state):
    """Handle a 'warehouse.deleted' event.

    Bumps the running 'deleted' counter for warehouses in `state` and remembers
    the id of the last warehouse that was deleted. Returns the new count.
    """
    bucket = state.setdefault("warehouse", {})
    count = bucket.get("deleted", 0) + 1
    bucket["deleted"] = count
    if "id" in record:
        bucket["last_deleted_id"] = record["id"]
    return count


def handle_warehouse_archived(record, state):
    """Handle a 'warehouse.archived' event.

    Bumps the running 'archived' counter for warehouses in `state` and remembers
    the id of the last warehouse that was archived. Returns the new count.
    """
    bucket = state.setdefault("warehouse", {})
    count = bucket.get("archived", 0) + 1
    bucket["archived"] = count
    if "id" in record:
        bucket["last_archived_id"] = record["id"]
    return count


def handle_warehouse_restored(record, state):
    """Handle a 'warehouse.restored' event.

    Bumps the running 'restored' counter for warehouses in `state` and remembers
    the id of the last warehouse that was restored. Returns the new count.
    """
    bucket = state.setdefault("warehouse", {})
    count = bucket.get("restored", 0) + 1
    bucket["restored"] = count
    if "id" in record:
        bucket["last_restored_id"] = record["id"]
    return count

# ---------------------------------------------------------------------------
# Collection helpers
# ---------------------------------------------------------------------------

def chunk(seq, size):
    """Split seq into consecutive lists of at most `size` items."""
    if size <= 0:
        raise ValueError("chunk size must be positive")
    return [list(seq[i:i + size]) for i in range(0, len(seq), size)]


def flatten(list_of_lists):
    """Flatten exactly one level of nesting."""
    out = []
    for inner in list_of_lists:
        out.extend(inner)
    return out


def dedupe(seq):
    """Remove duplicates, keeping first occurrences in order."""
    seen = set()
    out = []
    for item in seq:
        if item not in seen:
            seen.add(item)
            out.append(item)
    return out


def rotate(seq, k):
    """Rotate a list right by k positions (left for negative k)."""
    if not seq:
        return []
    k %= len(seq)
    return list(seq[-k:]) + list(seq[:-k]) if k else list(seq)


def partition(seq, pred):
    """Split into (matching, non_matching) by `pred`."""
    yes, no = [], []
    for item in seq:
        (yes if pred(item) else no).append(item)
    return yes, no


def group_by(seq, keyfn):
    """Group items into a dict of lists by `keyfn`."""
    out = {}
    for item in seq:
        out.setdefault(keyfn(item), []).append(item)
    return out


def index_by(seq, keyfn):
    """Map keyfn(item) -> item; later items win on collisions."""
    return {keyfn(item): item for item in seq}


def top_n(seq, n, keyfn=None):
    """The n largest items, biggest first."""
    return sorted(seq, key=keyfn, reverse=True)[:n]


def pick(mapping, keys):
    """Sub-dict of `mapping` with only `keys` (missing keys skipped)."""
    return {k: mapping[k] for k in keys if k in mapping}


def omit(mapping, keys):
    """Copy of `mapping` without `keys`."""
    drop = set(keys)
    return {k: v for k, v in mapping.items() if k not in drop}


def invert(mapping):
    """Swap keys and values; later duplicates win."""
    return {v: k for k, v in mapping.items()}


def rename_keys(mapping, renames):
    """Copy `mapping`, renaming keys via the `renames` dict."""
    return {renames.get(k, k): v for k, v in mapping.items()}


def zip_dicts(keys, values):
    """Pair two equal-length sequences into a dict."""
    if len(keys) != len(values):
        raise ValueError("length mismatch")
    return dict(zip(keys, values))


def tally(seq):
    """Count occurrences into a dict."""
    out = {}
    for item in seq:
        out[item] = out.get(item, 0) + 1
    return out


def interleave(a, b):
    """Alternate items from a and b, then append the leftovers."""
    out = []
    for x, y in zip(a, b):
        out.append(x)
        out.append(y)
    longer = a if len(a) > len(b) else b
    out.extend(longer[min(len(a), len(b)):])
    return out


def pairwise_diffs(values):
    """Difference between each consecutive pair of numbers."""
    return [b - a for a, b in zip(values, values[1:])]


def clamp(value, lo, hi):
    """Constrain `value` into [lo, hi]."""
    if lo > hi:
        raise ValueError("lo must be <= hi")
    return max(lo, min(hi, value))


def argmax(values):
    """Index of the largest value (first on ties); None for empty input."""
    if not values:
        return None
    best = 0
    for i, v in enumerate(values):
        if v > values[best]:
            best = i
    return best

# ---------------------------------------------------------------------------
# Light-weight parsers
# ---------------------------------------------------------------------------

def parse_bool(text):
    """Parse common boolean spellings; raise ValueError otherwise."""
    lowered = text.strip().lower()
    if lowered in ("1", "true", "yes", "on"):
        return True
    if lowered in ("0", "false", "no", "off"):
        return False
    raise ValueError(f"not a boolean: {text!r}")


def parse_int_strict(text):
    """Parse a decimal integer, rejecting floats and stray characters."""
    stripped = text.strip()
    if not re.fullmatch(r"[+-]?\d+", stripped):
        raise ValueError(f"not an integer: {text!r}")
    return int(stripped)


def parse_kv_line(line, sep="="):
    """Split "key=value" into a (key, value) tuple, trimming whitespace."""
    if sep not in line:
        raise ValueError(f"missing {sep!r} in {line!r}")
    key, _, value = line.partition(sep)
    return key.strip(), value.strip()


def parse_kv_block(text, sep="="):
    """Parse newline-separated key=value pairs, skipping blanks and #comments."""
    out = {}
    for line in text.split("\n"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, value = parse_kv_line(line, sep)
        out[key] = value
    return out


def parse_csv_row(line):
    """Naive CSV split (no quoting), trimming each cell."""
    return [cell.strip() for cell in line.split(",")]


def parse_range(text):
    """Parse "3-7" or "5" into an inclusive (lo, hi) tuple."""
    m = re.fullmatch(r"(\d+)(?:-(\d+))?", text.strip())
    if not m:
        raise ValueError(f"bad range: {text!r}")
    lo = int(m.group(1))
    hi = int(m.group(2)) if m.group(2) else lo
    if hi < lo:
        raise ValueError(f"inverted range: {text!r}")
    return lo, hi


def parse_tags(text):
    """Split a comma-separated tag list, dropping empties and duplicates."""
    return dedupe(t.strip() for t in text.split(",") if t.strip())


def parse_version(text):
    """Parse "1.2.3" into an (int, int, int) tuple."""
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", text.strip())
    if not m:
        raise ValueError(f"bad version: {text!r}")
    return tuple(int(g) for g in m.groups())
