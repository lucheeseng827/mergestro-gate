"""Known-positive fixture for the Slop Filter gate (Python).

`is_adult` has a boundary at 18. The test exercises it but never asserts the
boundary — a theater test. cosmic-ray's `>=`→`>` (and the other comparison /
number mutations on that line) survive, because no assertion depends on the
result. The Python adapter should surface exactly these.
"""


def is_adult(age):
    return age >= 18
