import json

def read_config(path):
    with open(path) as f:
        return json.load(f)
