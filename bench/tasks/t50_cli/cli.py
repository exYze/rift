def parse_args(argv):
    out = {}
    i = 0
    while i < len(argv):
        key = argv[i].lstrip('-')
        out[key] = argv[i + 1]
        i += 2
    return out
