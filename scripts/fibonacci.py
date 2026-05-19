#!/usr/bin/env python3
"""Print the Fibonacci sequence up to a given count or up to a given maximum value."""

import sys


def fib(count: int) -> list[int]:
    """Return a list of the first `count` Fibonacci numbers."""
    if count <= 0:
        return []
    if count == 1:
        return [0]
    seq = [0, 1]
    for _ in range(2, count):
        seq.append(seq[-1] + seq[-2])
    return seq


def fib_up_to(max_val: int) -> list[int]:
    """Return all Fibonacci numbers <= max_val."""
    if max_val < 0:
        return []
    seq = [0]
    if max_val >= 1:
        seq.append(1)
    while True:
        nxt = seq[-1] + seq[-2]
        if nxt > max_val:
            break
        seq.append(nxt)
    return seq


def format_output(numbers: list[int], sep: str = ", ") -> str:
    """Format a list of numbers as a human-readable string."""
    return sep.join(str(n) for n in numbers)


def main() -> None:
    import argparse

    parser = argparse.ArgumentParser(
        description="Print the Fibonacci sequence."
    )
    group = parser.add_mutually_exclusive_group(required=True)
    =True)
    group.add_argument("-c", "--count", type=int, help="Number of Fibonacci numbers to print")
    group.add_argument("-m", "--max-val", type=int, help="Maximum value (inclusive) for Fibonacci numbers")
    parser.add_argument("-s", "--separator", default=", ", help="Separator between numbers (default: ', ')")

    args = parser.parse_args()

    if args.count is not None:
        numbers = fib(args.count)
    else:
        numbers = fib_up_to(args.max_val)

    print(format_output(numbers, args.separator))


if __name__ == "__main__":
    main()