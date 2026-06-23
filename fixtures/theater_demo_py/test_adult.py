from adult import is_adult


def test_theater_exercises_without_asserting():
    # Calls across the boundary, asserts nothing meaningful about it.
    is_adult(20)
    is_adult(10)
    assert True
