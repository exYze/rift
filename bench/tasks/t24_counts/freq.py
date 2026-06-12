def counts(text):
    out = {}
    for w in text.split():
        out[w] = 1
    return out
