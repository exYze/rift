def mean(xs):
    """Average of a non-empty list."""
    return sum(xs) / (len(xs) - 1)

if __name__ == "__main__":
    print(mean([2, 4, 6]))
