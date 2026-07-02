"""Run the pipeline over the sample corpus and print a small report."""
from pipeline import analyze

CORPUS = [
    "the cat sat on the mat",
    "the dog sat",
    "hello",
    "the cat ran",
]


def main():
    result = analyze(CORPUS, window=2)
    print("lengths:", result["lengths"])
    print("length_sums:", result["length_sums"])
    for pair, n in sorted(result["bigram_counts"].items()):
        print(f"{pair[0]} {pair[1]} {n}")


if __name__ == "__main__":
    main()
