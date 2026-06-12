def deep_get(d, path, default=None):
    cur = d
    for k in path.split('.'):
        cur = cur[k]
    return cur
