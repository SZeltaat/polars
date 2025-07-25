from __future__ import annotations

from datetime import date, datetime
from typing import TYPE_CHECKING
from zoneinfo import ZoneInfo

import hypothesis.strategies as st
import pytest
from hypothesis import given

import polars as pl
from polars.exceptions import ComputeError, InvalidOperationError
from polars.testing import assert_series_equal
from polars.testing.asserts.frame import assert_frame_equal

if TYPE_CHECKING:
    from hypothesis.strategies import DrawFn

    from polars._typing import TimeUnit


DATE_FORMATS = ["%Y{}%m{}%d", "%d{}%m{}%Y"]
SEPARATORS = ["-", "/", "."]
TIME_FORMATS = [
    "T%H:%M:%S",
    "T%H%M%S",
    "T%H:%M",
    "T%H%M",
    " %H:%M:%S",
    " %H%M%S",
    " %H:%M",
    " %H%M",
    "",  # allow no time part (plain date)
]
FRACTIONS = [
    "%.9f",
    "%.6f",
    "%.3f",
    # "%.f", # alternative which allows any number of digits
    "",
]
TIMEZONES = ["%z", "%:z", "%:::z", ""]
DATETIME_PATTERNS = [
    date_format.format(separator, separator) + time_format + fraction + tz
    for separator in SEPARATORS
    for date_format in DATE_FORMATS
    for time_format in TIME_FORMATS
    for fraction in FRACTIONS
    if time_format.endswith("%S") or fraction == ""
    for tz in TIMEZONES
    if (date_format.startswith("%Y") and time_format != "") or tz == ""
]


@pytest.mark.parametrize("fmt", DATETIME_PATTERNS)
def test_to_datetime_inferable_formats(fmt: str) -> None:
    time_string = (
        fmt.replace("%Y", "2024")
        .replace("%m", "12")
        .replace("%d", "13")
        .replace("%H", "23")
        .replace("%M", "34")
        .replace("%S", "45")
        .replace("%.3f", ".123")
        .replace("%.6f", ".123456")
        .replace("%.9f", ".123456789")
        .replace("%z", "+0100")
        .replace("%:z", "+01:00")
        .replace("%:::z", "+01")
    )

    pl.Series([time_string]).str.to_datetime(strict=True)


@st.composite
def datetime_formats(draw: DrawFn) -> str:
    """Returns a strategy which generates datetime format strings."""
    parts = [
        "%m",
        "%b",
        "%B",
        "%d",
        "%j",
        "%a",
        "%A",
        "%w",
        "%H",
        "%I",
        "%p",
        "%M",
        "%S",
        "%U",
        "%W",
        "%%",
    ]
    fmt = draw(st.sets(st.sampled_from(parts)))
    fmt.add("%Y")  # Make sure year is always present
    return " ".join(fmt)


@given(
    datetimes=st.datetimes(
        min_value=datetime(1699, 1, 1),
        max_value=datetime(9999, 12, 31),
    ),
    fmt=datetime_formats(),
)
def test_to_datetime(datetimes: datetime, fmt: str) -> None:
    input = datetimes.strftime(fmt)
    expected = datetime.strptime(input, fmt)
    try:
        result = pl.Series([input]).str.to_datetime(format=fmt).item()
    # If there's an exception, check that it's either:
    # - something which polars can't parse at all: missing day or month
    # - something on which polars intentionally raises
    except InvalidOperationError as exc:
        assert "failed in column" in str(exc)  # noqa: PT017
        assert not any(day in fmt for day in ("%d", "%j")) or not any(
            month in fmt for month in ("%b", "%B", "%m")
        )
    except ComputeError as exc:
        assert "Invalid format string" in str(exc)  # noqa: PT017
        assert (
            (("%H" in fmt) ^ ("%M" in fmt))
            or (("%I" in fmt) ^ ("%M" in fmt))
            or ("%S" in fmt and "%H" not in fmt)
            or ("%S" in fmt and "%I" not in fmt)
            or (("%I" in fmt) ^ ("%p" in fmt))
            or (("%H" in fmt) ^ ("%p" in fmt))
        )
    else:
        assert result == expected


@given(
    d=st.datetimes(
        min_value=datetime(1699, 1, 1),
        max_value=datetime(9999, 12, 31),
    ),
    tu=st.sampled_from(["ms", "us"]),
)
def test_cast_to_time_and_combine(d: datetime, tu: TimeUnit) -> None:
    # round-trip date/time extraction + recombining
    df = pl.DataFrame({"d": [d]}, schema={"d": pl.Datetime(tu)})
    res = df.select(
        d=pl.col("d"),
        dt=pl.col("d").dt.date(),
        tm=pl.col("d").cast(pl.Time),
    ).with_columns(
        dtm=pl.col("dt").dt.combine(pl.col("tm")),
    )

    datetimes = res["d"].to_list()
    assert [d.date() for d in datetimes] == res["dt"].to_list()
    assert [d.time() for d in datetimes] == res["tm"].to_list()
    assert datetimes == res["dtm"].to_list()


def test_to_datetime_aware_values_aware_dtype() -> None:
    s = pl.Series(["2020-01-01T01:12:34+01:00"])
    expected = pl.Series([datetime(2020, 1, 1, 5, 57, 34)]).dt.replace_time_zone(
        "Asia/Kathmandu"
    )

    # When Polars infers the format
    result = s.str.to_datetime(time_zone="Asia/Kathmandu")
    assert_series_equal(result, expected)

    # When the format is provided
    result = s.str.to_datetime(format="%Y-%m-%dT%H:%M:%S%z", time_zone="Asia/Kathmandu")
    assert_series_equal(result, expected)

    # With `exact=False`
    result = s.str.to_datetime(
        format="%Y-%m-%dT%H:%M:%S%z", time_zone="Asia/Kathmandu", exact=False
    )
    assert_series_equal(result, expected)

    # Check consistency with Series constructor
    result = pl.Series(
        [datetime(2020, 1, 1, 5, 57, 34, tzinfo=ZoneInfo("Asia/Kathmandu"))],
        dtype=pl.Datetime("us", "Asia/Kathmandu"),
    )
    assert_series_equal(result, expected)


@pytest.mark.parametrize(
    ("inputs", "format", "expected"),
    [
        ("01-01-69", "%d-%m-%y", date(2069, 1, 1)),  # Polars' parser
        ("01-01-70", "%d-%m-%y", date(1970, 1, 1)),  # Polars' parser
        ("01-January-69", "%d-%B-%y", date(2069, 1, 1)),  # Chrono
        ("01-January-70", "%d-%B-%y", date(1970, 1, 1)),  # Chrono
    ],
)
def test_to_datetime_two_digit_year_17213(
    inputs: str, format: str, expected: date
) -> None:
    result = pl.Series([inputs]).str.to_date(format=format).item()
    assert result == expected


def test_to_datetime_column_input_to_ambiguous() -> None:
    q = pl.LazyFrame(
        {
            "a": ["2020-01-01 01:00Z", "2020-01-01 02:00Z"],
            "b": ["raise", "earliest"],
        }
    ).select(pl.col.a.str.to_datetime("%Y-%m-%d %H:%M%#z", ambiguous=pl.col.b))

    expect = pl.DataFrame(
        [
            pl.Series(
                "a",
                [
                    datetime(2020, 1, 1, 1, 0, tzinfo=ZoneInfo(key="UTC")),
                    datetime(2020, 1, 1, 2, 0, tzinfo=ZoneInfo(key="UTC")),
                ],
                dtype=pl.Datetime(time_unit="us", time_zone="UTC"),
            ),
        ]
    )

    assert_frame_equal(q.collect(), expect)


def test_to_datetime_fallible_predicate_pushdown() -> None:
    df = pl.DataFrame({"x": ["2020-10-25 01:00", "X"]})

    c = pl.first()

    expect_fail = [
        c.str.to_datetime(
            "%Y-%m-%d %H:%M",
            time_zone="Europe/London",
            ambiguous="raise",
            strict=False,
        ),
        c.str.to_datetime(
            "%Y-%m-%d %H:%M",
            time_zone="Europe/London",
            ambiguous="null",
            strict=True,
        ),
        c.str.to_datetime("%Y-%m-%d %H:%M", strict=True),
    ]

    expect_pass = [
        c.str.to_datetime(
            "%Y-%m-%d %H:%M",
            time_zone="Europe/London",
            ambiguous="null",
            strict=False,
        ),
        c.str.to_datetime(
            "%Y-%m-%d %H:%M",
            time_zone="Europe/London",
            ambiguous="earliest",
            strict=False,
        ),
        c.str.to_datetime(
            "%Y-%m-%d %H:%M",
            time_zone="Europe/London",
            ambiguous="latest",
            strict=False,
        ),
    ]

    for expr in expect_fail:
        with pytest.raises(Exception):  # noqa: B017
            df.select(expr)

    for expr in expect_pass:
        df.select(expr)

    lf = df.with_columns(false=False).lazy().filter("false")

    for e in expect_pass:
        q = lf.filter(e.is_not_null())
        plan = q.explain()
        assert plan.count("FILTER") == 1
        assert_frame_equal(
            q.collect(), q.collect(optimizations=pl.QueryOptFlags.none())
        )

    for e in expect_fail:
        q = lf.filter(e.is_not_null())
        plan = q.explain()
        assert_frame_equal(
            q.collect(), q.collect(optimizations=pl.QueryOptFlags.none())
        )
