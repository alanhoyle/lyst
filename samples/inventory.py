#!/usr/bin/env python3
"""A small inventory tracker, for trying lyst's syntax highlighting.

Reads people.csv-style data, keeps stock levels, and prints a report.
"""

from __future__ import annotations

import csv
import sys
from dataclasses import dataclass, field
from decimal import Decimal
from pathlib import Path
from typing import Iterator

LOW_STOCK = 5
CURRENCY = "€"


class OutOfStock(Exception):
    """Raised when more items are taken than are on hand."""


@dataclass(order=True)
class Item:
    name: str
    price: Decimal
    stock: int = 0
    tags: list[str] = field(default_factory=list)

    @property
    def value(self) -> Decimal:
        return self.price * self.stock

    def take(self, n: int = 1) -> None:
        if n > self.stock:
            raise OutOfStock(f"only {self.stock} {self.name!r} left, wanted {n}")
        self.stock -= n


def load(path: Path) -> Iterator[Item]:
    """Yield items from a semicolon-separated file with a header row."""
    with path.open(newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f, delimiter=";"):
            # European prices use a decimal comma: "2,50".
            price = Decimal(row["price"].replace(",", "."))
            yield Item(row["product"], price, int(row["stock"]))


def report(items: list[Item]) -> str:
    lines = [f"{'Item':<12}{'Price':>8}{'Stock':>7}"]
    for item in sorted(items, key=lambda i: (-i.value, i.name)):
        flag = "  ⚠ low" if 0 < item.stock <= LOW_STOCK else ""
        flag = "  ✗ out" if item.stock == 0 else flag
        lines.append(f"{item.name:<12}{CURRENCY}{item.price:>7.2f}{item.stock:>7}{flag}")
    total = sum((i.value for i in items), start=Decimal(0))
    lines.append(f"\nTotal value: {CURRENCY}{total:,.2f}")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    path = Path(argv[1]) if len(argv) > 1 else Path(__file__).with_name("semicolons.txt")
    items = list(load(path))
    try:
        items[0].take(3)
        items[-1].take()
    except OutOfStock as e:
        print(f"warning: {e}", file=sys.stderr)
    print(report(items))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
