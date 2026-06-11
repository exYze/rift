from client import fetch_data

def report(url):
    return fetch_data(url).upper()

if __name__ == "__main__":
    print(report("x"))
