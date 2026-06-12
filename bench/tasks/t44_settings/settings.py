DEFAULTS = {'theme': 'light', 'font': 'mono'}

def effective(user):
    merged = dict(user)
    merged.update(DEFAULTS)
    return merged
