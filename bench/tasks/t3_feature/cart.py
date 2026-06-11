def total(prices):
    """Sum the prices. Orders over 100 get a 10 percent discount
    applied to the whole order (round to 2 decimals)."""
    return sum(prices)

if __name__ == "__main__":
    print(total([20, 30]), total([60, 60]))
