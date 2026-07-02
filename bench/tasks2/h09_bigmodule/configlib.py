"""configlib: layered configuration for the service fleet.

Every service starts from the module-level DEFAULTS and overlays one or
more profile dicts on top. This module holds the per-section defaults,
validators, normalisers and change handlers, plus the merge machinery
that combines layers. Pure stdlib, no dependencies.
"""
import os
import re


# ---------------------------------------------------------------------------
# Environment coercion helpers
# ---------------------------------------------------------------------------

def env_str(name, default=""):
    """Read a string from the environment."""
    return os.environ.get(name, default)


def env_int(name, default=0):
    """Read an integer from the environment; raise ValueError on junk."""
    raw = os.environ.get(name)
    if raw is None:
        return default
    return int(raw.strip())


def env_float(name, default=0.0):
    """Read a float from the environment; raise ValueError on junk."""
    raw = os.environ.get(name)
    if raw is None:
        return default
    return float(raw.strip())


def env_bool(name, default=False):
    """Read a boolean from the environment (1/true/yes/on)."""
    raw = os.environ.get(name)
    if raw is None:
        return default
    return raw.strip().lower() in ("1", "true", "yes", "on")


def env_list(name, default=(), sep=","):
    """Read a separated list from the environment, trimming entries."""
    raw = os.environ.get(name)
    if raw is None:
        return list(default)
    return [part.strip() for part in raw.split(sep) if part.strip()]


def env_path(name, default=""):
    """Read a filesystem path from the environment, user-expanded."""
    raw = os.environ.get(name, default)
    return os.path.expanduser(raw) if raw else raw

# ---------------------------------------------------------------------------
# Key-path helpers
# ---------------------------------------------------------------------------

def flatten_config(config, prefix="", sep="."):
    """Flatten nested dicts into {"section.key": value} form."""
    out = {}
    for key, value in config.items():
        path = f"{prefix}{sep}{key}" if prefix else key
        if isinstance(value, dict):
            out.update(flatten_config(value, path, sep))
        else:
            out[path] = value
    return out


def expand_config(flat, sep="."):
    """Inverse of flatten_config()."""
    out = {}
    for path, value in flat.items():
        parts = path.split(sep)
        node = out
        for part in parts[:-1]:
            node = node.setdefault(part, {})
        node[parts[-1]] = value
    return out


def get_path(config, path, default=None, sep="."):
    """Fetch a dotted path like "database.port" out of a nested config."""
    node = config
    for part in path.split(sep):
        if not isinstance(node, dict) or part not in node:
            return default
        node = node[part]
    return node


def set_path(config, path, value, sep="."):
    """Set a dotted path in a nested config, creating dicts as needed."""
    parts = path.split(sep)
    node = config
    for part in parts[:-1]:
        node = node.setdefault(part, {})
    node[parts[-1]] = value
    return config


def diff_configs(old, new):
    """Flat {path: (old, new)} of every leaf that differs."""
    flat_old = flatten_config(old)
    flat_new = flatten_config(new)
    out = {}
    for path in sorted(set(flat_old) | set(flat_new)):
        if flat_old.get(path) != flat_new.get(path):
            out[path] = (flat_old.get(path), flat_new.get(path))
    return out

# ---------------------------------------------------------------------------
# Per-section defaults, validators, normalisers and change handlers
# ---------------------------------------------------------------------------


def default_logging():
    """Built-in defaults for the [logging] section (log routing)."""
    return {
        'enabled': True,
        'level': 'info',
        'targets': ['stderr'],
        'flush_interval': 30,
    }


def validate_logging(config):
    """Check the [logging] section of a full config; return problem strings."""
    section = config.get("logging", {})
    problems = []
    if not isinstance(section, dict):
        return ["logging: section must be a mapping"]
    allowed = default_logging()
    for key in allowed:
        if key not in section:
            problems.append("logging: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("logging: unknown key %r" % key)
    return problems


def normalize_logging(section):
    """Fill missing [logging] keys from the defaults; returns a new dict."""
    merged = default_logging()
    merged.update(section or {})
    return merged


def handle_logging_change(old, new, log):
    """Record which [logging] keys changed between two config versions."""
    old_section = old.get("logging", {})
    new_section = new.get("logging", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("logging", tuple(changed)))
    return changed


def default_database():
    """Built-in defaults for the [database] section (primary datastore)."""
    return {
        'host': 'localhost',
        'port': 5432,
        'name': 'app',
        'pool_size': 8,
        'timeout_ms': 5000,
    }


def validate_database(config):
    """Check the [database] section of a full config; return problem strings."""
    section = config.get("database", {})
    problems = []
    if not isinstance(section, dict):
        return ["database: section must be a mapping"]
    allowed = default_database()
    for key in allowed:
        if key not in section:
            problems.append("database: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("database: unknown key %r" % key)
    return problems


def normalize_database(section):
    """Fill missing [database] keys from the defaults; returns a new dict."""
    merged = default_database()
    merged.update(section or {})
    return merged


def handle_database_change(old, new, log):
    """Record which [database] keys changed between two config versions."""
    old_section = old.get("database", {})
    new_section = new.get("database", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("database", tuple(changed)))
    return changed


def default_cache():
    """Built-in defaults for the [cache] section (read-through cache)."""
    return {
        'backend': 'memory',
        'ttl_seconds': 300,
        'max_entries': 10000,
    }


def validate_cache(config):
    """Check the [cache] section of a full config; return problem strings."""
    section = config.get("cache", {})
    problems = []
    if not isinstance(section, dict):
        return ["cache: section must be a mapping"]
    allowed = default_cache()
    for key in allowed:
        if key not in section:
            problems.append("cache: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("cache: unknown key %r" % key)
    return problems


def normalize_cache(section):
    """Fill missing [cache] keys from the defaults; returns a new dict."""
    merged = default_cache()
    merged.update(section or {})
    return merged


def handle_cache_change(old, new, log):
    """Record which [cache] keys changed between two config versions."""
    old_section = old.get("cache", {})
    new_section = new.get("cache", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("cache", tuple(changed)))
    return changed


def default_auth():
    """Built-in defaults for the [auth] section (authentication)."""
    return {
        'provider': 'local',
        'session_minutes': 60,
        'mfa_required': False,
        'lockout_attempts': 5,
    }


def validate_auth(config):
    """Check the [auth] section of a full config; return problem strings."""
    section = config.get("auth", {})
    problems = []
    if not isinstance(section, dict):
        return ["auth: section must be a mapping"]
    allowed = default_auth()
    for key in allowed:
        if key not in section:
            problems.append("auth: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("auth: unknown key %r" % key)
    return problems


def normalize_auth(section):
    """Fill missing [auth] keys from the defaults; returns a new dict."""
    merged = default_auth()
    merged.update(section or {})
    return merged


def handle_auth_change(old, new, log):
    """Record which [auth] keys changed between two config versions."""
    old_section = old.get("auth", {})
    new_section = new.get("auth", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("auth", tuple(changed)))
    return changed


def default_server():
    """Built-in defaults for the [server] section (HTTP front end)."""
    return {
        'bind': '0.0.0.0',
        'port': 8080,
        'workers': 4,
        'graceful_timeout': 15,
    }


def validate_server(config):
    """Check the [server] section of a full config; return problem strings."""
    section = config.get("server", {})
    problems = []
    if not isinstance(section, dict):
        return ["server: section must be a mapping"]
    allowed = default_server()
    for key in allowed:
        if key not in section:
            problems.append("server: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("server: unknown key %r" % key)
    return problems


def normalize_server(section):
    """Fill missing [server] keys from the defaults; returns a new dict."""
    merged = default_server()
    merged.update(section or {})
    return merged


def handle_server_change(old, new, log):
    """Record which [server] keys changed between two config versions."""
    old_section = old.get("server", {})
    new_section = new.get("server", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("server", tuple(changed)))
    return changed


def default_metrics():
    """Built-in defaults for the [metrics] section (telemetry export)."""
    return {
        'enabled': True,
        'interval_seconds': 10,
        'prefix': 'app',
        'histogram_buckets': [5, 50, 500],
    }


def validate_metrics(config):
    """Check the [metrics] section of a full config; return problem strings."""
    section = config.get("metrics", {})
    problems = []
    if not isinstance(section, dict):
        return ["metrics: section must be a mapping"]
    allowed = default_metrics()
    for key in allowed:
        if key not in section:
            problems.append("metrics: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("metrics: unknown key %r" % key)
    return problems


def normalize_metrics(section):
    """Fill missing [metrics] keys from the defaults; returns a new dict."""
    merged = default_metrics()
    merged.update(section or {})
    return merged


def handle_metrics_change(old, new, log):
    """Record which [metrics] keys changed between two config versions."""
    old_section = old.get("metrics", {})
    new_section = new.get("metrics", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("metrics", tuple(changed)))
    return changed


def default_tracing():
    """Built-in defaults for the [tracing] section (distributed tracing)."""
    return {
        'enabled': False,
        'sample_rate_pct': 1,
        'exporter': 'none',
    }


def validate_tracing(config):
    """Check the [tracing] section of a full config; return problem strings."""
    section = config.get("tracing", {})
    problems = []
    if not isinstance(section, dict):
        return ["tracing: section must be a mapping"]
    allowed = default_tracing()
    for key in allowed:
        if key not in section:
            problems.append("tracing: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("tracing: unknown key %r" % key)
    return problems


def normalize_tracing(section):
    """Fill missing [tracing] keys from the defaults; returns a new dict."""
    merged = default_tracing()
    merged.update(section or {})
    return merged


def handle_tracing_change(old, new, log):
    """Record which [tracing] keys changed between two config versions."""
    old_section = old.get("tracing", {})
    new_section = new.get("tracing", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("tracing", tuple(changed)))
    return changed


def default_email():
    """Built-in defaults for the [email] section (outbound mail)."""
    return {
        'transport': 'smtp',
        'host': 'localhost',
        'port': 25,
        'from_address': 'noreply@example.com',
    }


def validate_email(config):
    """Check the [email] section of a full config; return problem strings."""
    section = config.get("email", {})
    problems = []
    if not isinstance(section, dict):
        return ["email: section must be a mapping"]
    allowed = default_email()
    for key in allowed:
        if key not in section:
            problems.append("email: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("email: unknown key %r" % key)
    return problems


def normalize_email(section):
    """Fill missing [email] keys from the defaults; returns a new dict."""
    merged = default_email()
    merged.update(section or {})
    return merged


def handle_email_change(old, new, log):
    """Record which [email] keys changed between two config versions."""
    old_section = old.get("email", {})
    new_section = new.get("email", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("email", tuple(changed)))
    return changed


def default_storage():
    """Built-in defaults for the [storage] section (blob storage)."""
    return {
        'driver': 'disk',
        'root': '/var/lib/app',
        'quota_mb': 1024,
    }


def validate_storage(config):
    """Check the [storage] section of a full config; return problem strings."""
    section = config.get("storage", {})
    problems = []
    if not isinstance(section, dict):
        return ["storage: section must be a mapping"]
    allowed = default_storage()
    for key in allowed:
        if key not in section:
            problems.append("storage: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("storage: unknown key %r" % key)
    return problems


def normalize_storage(section):
    """Fill missing [storage] keys from the defaults; returns a new dict."""
    merged = default_storage()
    merged.update(section or {})
    return merged


def handle_storage_change(old, new, log):
    """Record which [storage] keys changed between two config versions."""
    old_section = old.get("storage", {})
    new_section = new.get("storage", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("storage", tuple(changed)))
    return changed


def default_queue():
    """Built-in defaults for the [queue] section (background jobs)."""
    return {
        'broker': 'memory',
        'prefetch': 16,
        'retry_limit': 3,
        'dead_letter': True,
    }


def validate_queue(config):
    """Check the [queue] section of a full config; return problem strings."""
    section = config.get("queue", {})
    problems = []
    if not isinstance(section, dict):
        return ["queue: section must be a mapping"]
    allowed = default_queue()
    for key in allowed:
        if key not in section:
            problems.append("queue: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("queue: unknown key %r" % key)
    return problems


def normalize_queue(section):
    """Fill missing [queue] keys from the defaults; returns a new dict."""
    merged = default_queue()
    merged.update(section or {})
    return merged


def handle_queue_change(old, new, log):
    """Record which [queue] keys changed between two config versions."""
    old_section = old.get("queue", {})
    new_section = new.get("queue", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("queue", tuple(changed)))
    return changed


def default_search():
    """Built-in defaults for the [search] section (full-text search)."""
    return {
        'engine': 'builtin',
        'index_batch': 200,
        'refresh_seconds': 60,
    }


def validate_search(config):
    """Check the [search] section of a full config; return problem strings."""
    section = config.get("search", {})
    problems = []
    if not isinstance(section, dict):
        return ["search: section must be a mapping"]
    allowed = default_search()
    for key in allowed:
        if key not in section:
            problems.append("search: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("search: unknown key %r" % key)
    return problems


def normalize_search(section):
    """Fill missing [search] keys from the defaults; returns a new dict."""
    merged = default_search()
    merged.update(section or {})
    return merged


def handle_search_change(old, new, log):
    """Record which [search] keys changed between two config versions."""
    old_section = old.get("search", {})
    new_section = new.get("search", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("search", tuple(changed)))
    return changed


def default_scheduler():
    """Built-in defaults for the [scheduler] section (periodic tasks)."""
    return {
        'enabled': True,
        'tick_seconds': 5,
        'max_concurrent': 10,
    }


def validate_scheduler(config):
    """Check the [scheduler] section of a full config; return problem strings."""
    section = config.get("scheduler", {})
    problems = []
    if not isinstance(section, dict):
        return ["scheduler: section must be a mapping"]
    allowed = default_scheduler()
    for key in allowed:
        if key not in section:
            problems.append("scheduler: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("scheduler: unknown key %r" % key)
    return problems


def normalize_scheduler(section):
    """Fill missing [scheduler] keys from the defaults; returns a new dict."""
    merged = default_scheduler()
    merged.update(section or {})
    return merged


def handle_scheduler_change(old, new, log):
    """Record which [scheduler] keys changed between two config versions."""
    old_section = old.get("scheduler", {})
    new_section = new.get("scheduler", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("scheduler", tuple(changed)))
    return changed


def default_rate_limit():
    """Built-in defaults for the [rate_limit] section (request throttling)."""
    return {
        'enabled': True,
        'requests_per_minute': 600,
        'burst': 50,
    }


def validate_rate_limit(config):
    """Check the [rate_limit] section of a full config; return problem strings."""
    section = config.get("rate_limit", {})
    problems = []
    if not isinstance(section, dict):
        return ["rate_limit: section must be a mapping"]
    allowed = default_rate_limit()
    for key in allowed:
        if key not in section:
            problems.append("rate_limit: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("rate_limit: unknown key %r" % key)
    return problems


def normalize_rate_limit(section):
    """Fill missing [rate_limit] keys from the defaults; returns a new dict."""
    merged = default_rate_limit()
    merged.update(section or {})
    return merged


def handle_rate_limit_change(old, new, log):
    """Record which [rate_limit] keys changed between two config versions."""
    old_section = old.get("rate_limit", {})
    new_section = new.get("rate_limit", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("rate_limit", tuple(changed)))
    return changed


def default_session():
    """Built-in defaults for the [session] section (browser sessions)."""
    return {
        'cookie_name': 'sid',
        'secure': True,
        'same_site': 'lax',
        'idle_minutes': 30,
    }


def validate_session(config):
    """Check the [session] section of a full config; return problem strings."""
    section = config.get("session", {})
    problems = []
    if not isinstance(section, dict):
        return ["session: section must be a mapping"]
    allowed = default_session()
    for key in allowed:
        if key not in section:
            problems.append("session: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("session: unknown key %r" % key)
    return problems


def normalize_session(section):
    """Fill missing [session] keys from the defaults; returns a new dict."""
    merged = default_session()
    merged.update(section or {})
    return merged


def handle_session_change(old, new, log):
    """Record which [session] keys changed between two config versions."""
    old_section = old.get("session", {})
    new_section = new.get("session", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("session", tuple(changed)))
    return changed


def default_cors():
    """Built-in defaults for the [cors] section (cross-origin requests)."""
    return {
        'enabled': False,
        'origins': [],
        'allow_credentials': False,
    }


def validate_cors(config):
    """Check the [cors] section of a full config; return problem strings."""
    section = config.get("cors", {})
    problems = []
    if not isinstance(section, dict):
        return ["cors: section must be a mapping"]
    allowed = default_cors()
    for key in allowed:
        if key not in section:
            problems.append("cors: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("cors: unknown key %r" % key)
    return problems


def normalize_cors(section):
    """Fill missing [cors] keys from the defaults; returns a new dict."""
    merged = default_cors()
    merged.update(section or {})
    return merged


def handle_cors_change(old, new, log):
    """Record which [cors] keys changed between two config versions."""
    old_section = old.get("cors", {})
    new_section = new.get("cors", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("cors", tuple(changed)))
    return changed


def default_tls():
    """Built-in defaults for the [tls] section (transport security)."""
    return {
        'enabled': False,
        'cert_path': '',
        'key_path': '',
        'min_version': '1.2',
    }


def validate_tls(config):
    """Check the [tls] section of a full config; return problem strings."""
    section = config.get("tls", {})
    problems = []
    if not isinstance(section, dict):
        return ["tls: section must be a mapping"]
    allowed = default_tls()
    for key in allowed:
        if key not in section:
            problems.append("tls: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("tls: unknown key %r" % key)
    return problems


def normalize_tls(section):
    """Fill missing [tls] keys from the defaults; returns a new dict."""
    merged = default_tls()
    merged.update(section or {})
    return merged


def handle_tls_change(old, new, log):
    """Record which [tls] keys changed between two config versions."""
    old_section = old.get("tls", {})
    new_section = new.get("tls", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("tls", tuple(changed)))
    return changed


def default_backup():
    """Built-in defaults for the [backup] section (scheduled backups)."""
    return {
        'enabled': True,
        'hour_utc': 3,
        'keep_days': 14,
        'destination': 'local',
    }


def validate_backup(config):
    """Check the [backup] section of a full config; return problem strings."""
    section = config.get("backup", {})
    problems = []
    if not isinstance(section, dict):
        return ["backup: section must be a mapping"]
    allowed = default_backup()
    for key in allowed:
        if key not in section:
            problems.append("backup: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("backup: unknown key %r" % key)
    return problems


def normalize_backup(section):
    """Fill missing [backup] keys from the defaults; returns a new dict."""
    merged = default_backup()
    merged.update(section or {})
    return merged


def handle_backup_change(old, new, log):
    """Record which [backup] keys changed between two config versions."""
    old_section = old.get("backup", {})
    new_section = new.get("backup", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("backup", tuple(changed)))
    return changed


def default_alerts():
    """Built-in defaults for the [alerts] section (operator alerting)."""
    return {
        'enabled': True,
        'channel': 'log',
        'min_severity': 'warning',
    }


def validate_alerts(config):
    """Check the [alerts] section of a full config; return problem strings."""
    section = config.get("alerts", {})
    problems = []
    if not isinstance(section, dict):
        return ["alerts: section must be a mapping"]
    allowed = default_alerts()
    for key in allowed:
        if key not in section:
            problems.append("alerts: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("alerts: unknown key %r" % key)
    return problems


def normalize_alerts(section):
    """Fill missing [alerts] keys from the defaults; returns a new dict."""
    merged = default_alerts()
    merged.update(section or {})
    return merged


def handle_alerts_change(old, new, log):
    """Record which [alerts] keys changed between two config versions."""
    old_section = old.get("alerts", {})
    new_section = new.get("alerts", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("alerts", tuple(changed)))
    return changed


def default_features():
    """Built-in defaults for the [features] section (feature flags)."""
    return {
        'beta_ui': False,
        'bulk_export': True,
        'async_reports': False,
    }


def validate_features(config):
    """Check the [features] section of a full config; return problem strings."""
    section = config.get("features", {})
    problems = []
    if not isinstance(section, dict):
        return ["features: section must be a mapping"]
    allowed = default_features()
    for key in allowed:
        if key not in section:
            problems.append("features: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("features: unknown key %r" % key)
    return problems


def normalize_features(section):
    """Fill missing [features] keys from the defaults; returns a new dict."""
    merged = default_features()
    merged.update(section or {})
    return merged


def handle_features_change(old, new, log):
    """Record which [features] keys changed between two config versions."""
    old_section = old.get("features", {})
    new_section = new.get("features", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("features", tuple(changed)))
    return changed


def default_i18n():
    """Built-in defaults for the [i18n] section (localisation)."""
    return {
        'default_locale': 'en',
        'fallback_locale': 'en',
        'supported': ['en'],
    }


def validate_i18n(config):
    """Check the [i18n] section of a full config; return problem strings."""
    section = config.get("i18n", {})
    problems = []
    if not isinstance(section, dict):
        return ["i18n: section must be a mapping"]
    allowed = default_i18n()
    for key in allowed:
        if key not in section:
            problems.append("i18n: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("i18n: unknown key %r" % key)
    return problems


def normalize_i18n(section):
    """Fill missing [i18n] keys from the defaults; returns a new dict."""
    merged = default_i18n()
    merged.update(section or {})
    return merged


def handle_i18n_change(old, new, log):
    """Record which [i18n] keys changed between two config versions."""
    old_section = old.get("i18n", {})
    new_section = new.get("i18n", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("i18n", tuple(changed)))
    return changed


def default_webhooks():
    """Built-in defaults for the [webhooks] section (outbound webhooks)."""
    return {
        'enabled': False,
        'timeout_ms': 3000,
        'max_retries': 5,
        'sign_payloads': True,
    }


def validate_webhooks(config):
    """Check the [webhooks] section of a full config; return problem strings."""
    section = config.get("webhooks", {})
    problems = []
    if not isinstance(section, dict):
        return ["webhooks: section must be a mapping"]
    allowed = default_webhooks()
    for key in allowed:
        if key not in section:
            problems.append("webhooks: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("webhooks: unknown key %r" % key)
    return problems


def normalize_webhooks(section):
    """Fill missing [webhooks] keys from the defaults; returns a new dict."""
    merged = default_webhooks()
    merged.update(section or {})
    return merged


def handle_webhooks_change(old, new, log):
    """Record which [webhooks] keys changed between two config versions."""
    old_section = old.get("webhooks", {})
    new_section = new.get("webhooks", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("webhooks", tuple(changed)))
    return changed


def default_exports():
    """Built-in defaults for the [exports] section (data exports)."""
    return {
        'format': 'csv',
        'max_rows': 100000,
        'tmp_dir': '/tmp',
    }


def validate_exports(config):
    """Check the [exports] section of a full config; return problem strings."""
    section = config.get("exports", {})
    problems = []
    if not isinstance(section, dict):
        return ["exports: section must be a mapping"]
    allowed = default_exports()
    for key in allowed:
        if key not in section:
            problems.append("exports: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("exports: unknown key %r" % key)
    return problems


def normalize_exports(section):
    """Fill missing [exports] keys from the defaults; returns a new dict."""
    merged = default_exports()
    merged.update(section or {})
    return merged


def handle_exports_change(old, new, log):
    """Record which [exports] keys changed between two config versions."""
    old_section = old.get("exports", {})
    new_section = new.get("exports", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("exports", tuple(changed)))
    return changed


def default_sync():
    """Built-in defaults for the [sync] section (peer synchronisation)."""
    return {
        'enabled': False,
        'interval_minutes': 15,
        'peers': [],
    }


def validate_sync(config):
    """Check the [sync] section of a full config; return problem strings."""
    section = config.get("sync", {})
    problems = []
    if not isinstance(section, dict):
        return ["sync: section must be a mapping"]
    allowed = default_sync()
    for key in allowed:
        if key not in section:
            problems.append("sync: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("sync: unknown key %r" % key)
    return problems


def normalize_sync(section):
    """Fill missing [sync] keys from the defaults; returns a new dict."""
    merged = default_sync()
    merged.update(section or {})
    return merged


def handle_sync_change(old, new, log):
    """Record which [sync] keys changed between two config versions."""
    old_section = old.get("sync", {})
    new_section = new.get("sync", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("sync", tuple(changed)))
    return changed


def default_audit():
    """Built-in defaults for the [audit] section (audit trail)."""
    return {
        'enabled': True,
        'sink': 'database',
        'retain_days': 90,
    }


def validate_audit(config):
    """Check the [audit] section of a full config; return problem strings."""
    section = config.get("audit", {})
    problems = []
    if not isinstance(section, dict):
        return ["audit: section must be a mapping"]
    allowed = default_audit()
    for key in allowed:
        if key not in section:
            problems.append("audit: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("audit: unknown key %r" % key)
    return problems


def normalize_audit(section):
    """Fill missing [audit] keys from the defaults; returns a new dict."""
    merged = default_audit()
    merged.update(section or {})
    return merged


def handle_audit_change(old, new, log):
    """Record which [audit] keys changed between two config versions."""
    old_section = old.get("audit", {})
    new_section = new.get("audit", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("audit", tuple(changed)))
    return changed


def default_uploads():
    """Built-in defaults for the [uploads] section (user file uploads)."""
    return {
        'max_mb': 25,
        'allowed_types': ['png', 'jpg', 'pdf'],
        'scan_for_malware': True,
    }


def validate_uploads(config):
    """Check the [uploads] section of a full config; return problem strings."""
    section = config.get("uploads", {})
    problems = []
    if not isinstance(section, dict):
        return ["uploads: section must be a mapping"]
    allowed = default_uploads()
    for key in allowed:
        if key not in section:
            problems.append("uploads: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("uploads: unknown key %r" % key)
    return problems


def normalize_uploads(section):
    """Fill missing [uploads] keys from the defaults; returns a new dict."""
    merged = default_uploads()
    merged.update(section or {})
    return merged


def handle_uploads_change(old, new, log):
    """Record which [uploads] keys changed between two config versions."""
    old_section = old.get("uploads", {})
    new_section = new.get("uploads", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("uploads", tuple(changed)))
    return changed


def default_notifications():
    """Built-in defaults for the [notifications] section (user notifications)."""
    return {
        'digest_hour': 8,
        'channels': ['email'],
        'batch_size': 100,
    }


def validate_notifications(config):
    """Check the [notifications] section of a full config; return problem strings."""
    section = config.get("notifications", {})
    problems = []
    if not isinstance(section, dict):
        return ["notifications: section must be a mapping"]
    allowed = default_notifications()
    for key in allowed:
        if key not in section:
            problems.append("notifications: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("notifications: unknown key %r" % key)
    return problems


def normalize_notifications(section):
    """Fill missing [notifications] keys from the defaults; returns a new dict."""
    merged = default_notifications()
    merged.update(section or {})
    return merged


def handle_notifications_change(old, new, log):
    """Record which [notifications] keys changed between two config versions."""
    old_section = old.get("notifications", {})
    new_section = new.get("notifications", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("notifications", tuple(changed)))
    return changed


def default_pagination():
    """Built-in defaults for the [pagination] section (list endpoints)."""
    return {
        'default_page_size': 25,
        'max_page_size': 200,
    }


def validate_pagination(config):
    """Check the [pagination] section of a full config; return problem strings."""
    section = config.get("pagination", {})
    problems = []
    if not isinstance(section, dict):
        return ["pagination: section must be a mapping"]
    allowed = default_pagination()
    for key in allowed:
        if key not in section:
            problems.append("pagination: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("pagination: unknown key %r" % key)
    return problems


def normalize_pagination(section):
    """Fill missing [pagination] keys from the defaults; returns a new dict."""
    merged = default_pagination()
    merged.update(section or {})
    return merged


def handle_pagination_change(old, new, log):
    """Record which [pagination] keys changed between two config versions."""
    old_section = old.get("pagination", {})
    new_section = new.get("pagination", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("pagination", tuple(changed)))
    return changed


def default_healthcheck():
    """Built-in defaults for the [healthcheck] section (liveness probes)."""
    return {
        'path': '/healthz',
        'include_details': False,
        'timeout_ms': 500,
    }


def validate_healthcheck(config):
    """Check the [healthcheck] section of a full config; return problem strings."""
    section = config.get("healthcheck", {})
    problems = []
    if not isinstance(section, dict):
        return ["healthcheck: section must be a mapping"]
    allowed = default_healthcheck()
    for key in allowed:
        if key not in section:
            problems.append("healthcheck: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("healthcheck: unknown key %r" % key)
    return problems


def normalize_healthcheck(section):
    """Fill missing [healthcheck] keys from the defaults; returns a new dict."""
    merged = default_healthcheck()
    merged.update(section or {})
    return merged


def handle_healthcheck_change(old, new, log):
    """Record which [healthcheck] keys changed between two config versions."""
    old_section = old.get("healthcheck", {})
    new_section = new.get("healthcheck", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("healthcheck", tuple(changed)))
    return changed


def default_compression():
    """Built-in defaults for the [compression] section (response compression)."""
    return {
        'enabled': True,
        'algorithm': 'gzip',
        'min_bytes': 1024,
    }


def validate_compression(config):
    """Check the [compression] section of a full config; return problem strings."""
    section = config.get("compression", {})
    problems = []
    if not isinstance(section, dict):
        return ["compression: section must be a mapping"]
    allowed = default_compression()
    for key in allowed:
        if key not in section:
            problems.append("compression: missing key %r" % key)
    for key in section:
        if key not in allowed:
            problems.append("compression: unknown key %r" % key)
    return problems


def normalize_compression(section):
    """Fill missing [compression] keys from the defaults; returns a new dict."""
    merged = default_compression()
    merged.update(section or {})
    return merged


def handle_compression_change(old, new, log):
    """Record which [compression] keys changed between two config versions."""
    old_section = old.get("compression", {})
    new_section = new.get("compression", {})
    changed = []
    for key in sorted(set(old_section) | set(new_section)):
        if old_section.get(key) != new_section.get(key):
            changed.append(key)
    if changed:
        log.append(("compression", tuple(changed)))
    return changed

# ---------------------------------------------------------------------------
# The assembled defaults
# ---------------------------------------------------------------------------

def build_defaults():
    """A fresh, fully-populated defaults tree."""
    return {
        "logging": default_logging(),
        "database": default_database(),
        "cache": default_cache(),
        "auth": default_auth(),
        "server": default_server(),
        "metrics": default_metrics(),
        "tracing": default_tracing(),
        "email": default_email(),
        "storage": default_storage(),
        "queue": default_queue(),
        "search": default_search(),
        "scheduler": default_scheduler(),
        "rate_limit": default_rate_limit(),
        "session": default_session(),
        "cors": default_cors(),
        "tls": default_tls(),
        "backup": default_backup(),
        "alerts": default_alerts(),
        "features": default_features(),
        "i18n": default_i18n(),
        "webhooks": default_webhooks(),
        "exports": default_exports(),
        "sync": default_sync(),
        "audit": default_audit(),
        "uploads": default_uploads(),
        "notifications": default_notifications(),
        "pagination": default_pagination(),
        "healthcheck": default_healthcheck(),
        "compression": default_compression(),
    }


DEFAULTS = build_defaults()

# ---------------------------------------------------------------------------
# Layer merging
# ---------------------------------------------------------------------------

def merge_flat(base, override):
    """Shallow merge: a copy of `base` with `override`'s top-level keys."""
    out = dict(base)
    out.update(override)
    return out


def deep_merge(base, override):
    """Recursively merge two config dicts; values in `override` win.

    Nested dicts are merged key by key; any other override value
    replaces the base value wholesale. Neither input is modified.
    """
    out = base
    for key, value in override.items():
        if isinstance(value, dict) and isinstance(out.get(key), dict):
            out[key] = deep_merge(out[key], value)
        else:
            out[key] = value
    return out


def merge_many(layers):
    """deep_merge a sequence of layers, first to last."""
    merged = {}
    for layer in layers:
        merged = deep_merge(merged, layer)
    return merged


def apply_profile(profile):
    """Full config for one profile: DEFAULTS overlaid with `profile`."""
    return deep_merge(DEFAULTS, profile)


def apply_profiles(profiles):
    """Full config for a stack of profiles, first to last."""
    config = DEFAULTS
    for profile in profiles:
        config = deep_merge(config, profile)
    return config

# ---------------------------------------------------------------------------
# Misc utilities
# ---------------------------------------------------------------------------

_SECTION_NAME_RE = re.compile(r"[a-z][a-z0-9_]*")


def is_section_name(text):
    """True for a well-formed section name."""
    return isinstance(text, str) and bool(_SECTION_NAME_RE.fullmatch(text))


def known_sections():
    """Sorted list of every section DEFAULTS ships with."""
    return sorted(DEFAULTS)


def section_or_empty(config, section):
    """The named section dict, or {} when absent."""
    value = config.get(section, {})
    return value if isinstance(value, dict) else {}


def validate_all(config):
    """Run every per-section validator; return the combined problem list."""
    problems = []
    for section in sorted(DEFAULTS):
        checker = globals().get(f"validate_{section}")
        if checker is not None:
            problems.extend(checker(config))
    return problems


def normalize_all(config):
    """Fill every section of `config` out with its defaults (new dict)."""
    out = {}
    for section in sorted(DEFAULTS):
        filler = globals().get(f"normalize_{section}")
        raw = section_or_empty(config, section)
        out[section] = filler(raw) if filler is not None else dict(raw)
    return out


def summarize(config):
    """One-line-per-section summary, for debug logging."""
    lines = []
    for section in sorted(config):
        body = config[section]
        if isinstance(body, dict):
            lines.append(f"{section}: {len(body)} keys")
        else:
            lines.append(f"{section}: {body!r}")
    return "\n".join(lines)
