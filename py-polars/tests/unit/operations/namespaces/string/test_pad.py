from __future__ import annotations

import pytest

import polars as pl
from polars.exceptions import ShapeError
from polars.testing import assert_frame_equal


def test_str_pad_start() -> None:
    df = pl.DataFrame({"a": ["foo", "longer_foo", "longest_fooooooo", "hi"]})

    result = df.select(
        pl.col("a").str.pad_start(10).alias("padded"),
        pl.col("a").str.pad_start(10).str.len_bytes().alias("padded_len"),
    )

    expected = pl.DataFrame(
        {
            "padded": ["       foo", "longer_foo", "longest_fooooooo", "        hi"],
            "padded_len": [10, 10, 16, 10],
        },
        schema_overrides={"padded_len": pl.UInt32},
    )
    assert_frame_equal(result, expected)


def test_str_pad_start_expr() -> None:
    df = pl.DataFrame({"a": ["a", "bbbbbb", "cc", "d", None], "b": [1, 2, None, 4, 4]})
    result = df.select(
        lit_expr=pl.col("a").str.pad_start(pl.lit(4)),
        int_expr=pl.col("a").str.pad_start(4),
        b_expr=pl.col("a").str.pad_start("b"),
    )
    expected = pl.DataFrame(
        {
            "lit_expr": ["   a", "bbbbbb", "  cc", "   d", None],
            "int_expr": ["   a", "bbbbbb", "  cc", "   d", None],
            "b_expr": ["a", "bbbbbb", None, "   d", None],
        }
    )
    assert_frame_equal(result, expected)


def test_str_pad_end_expr() -> None:
    df = pl.DataFrame({"a": ["a", "bbbbbb", "cc", "d", None], "b": [1, 2, None, 4, 4]})
    result = df.select(
        lit_expr=pl.col("a").str.pad_end(pl.lit(4)),
        int_expr=pl.col("a").str.pad_end(4),
        b_expr=pl.col("a").str.pad_end("b"),
    )
    expected = pl.DataFrame(
        {
            "lit_expr": ["a   ", "bbbbbb", "cc  ", "d   ", None],
            "int_expr": ["a   ", "bbbbbb", "cc  ", "d   ", None],
            "b_expr": ["a", "bbbbbb", None, "d   ", None],
        }
    )
    assert_frame_equal(result, expected)


def test_str_pad_end() -> None:
    df = pl.DataFrame({"a": ["foo", "longer_foo", "longest_fooooooo", "hi"]})

    result = df.select(
        pl.col("a").str.pad_end(10).alias("padded"),
        pl.col("a").str.pad_end(10).str.len_bytes().alias("padded_len"),
    )

    expected = pl.DataFrame(
        {
            "padded": ["foo       ", "longer_foo", "longest_fooooooo", "hi        "],
            "padded_len": [10, 10, 16, 10],
        },
        schema_overrides={"padded_len": pl.UInt32},
    )
    assert_frame_equal(result, expected)


def test_str_zfill() -> None:
    df = pl.DataFrame(
        {
            "num": [-10, -1, 0, 1, 10, 100, 1000, 10000, 100000, 1000000, None],
        }
    )
    out = [
        "-0010",
        "-0001",
        "00000",
        "00001",
        "00010",
        "00100",
        "01000",
        "10000",
        "100000",
        "1000000",
        None,
    ]
    assert (
        df.with_columns(pl.col("num").cast(str).str.zfill(5)).to_series().to_list()
        == out
    )
    assert df["num"].cast(str).str.zfill(5).to_list() == out


def test_str_zfill_expr() -> None:
    df = pl.DataFrame(
        {
            "num": ["-10", "-1", "0", "1", "10", None, "1", "+1"],
            # u8 tests the IR length cast
            "len_u8": pl.Series([3, 4, 3, 2, 5, 3, None, 3], dtype=pl.UInt8),
            "len_u64": pl.Series([3, 4, 3, 2, 5, 3, None, 3], dtype=pl.UInt64),
        }
    )
    out = df.select(
        all_expr_u8=pl.col("num").str.zfill(pl.col("len_u8") + 1),
        all_expr=pl.col("num").str.zfill(pl.col("len_u64") + 1),
        str_lit=pl.lit("10").str.zfill(pl.col("len_u64")),
        len_lit=pl.col("num").str.zfill(5),
    )
    expected = pl.DataFrame(
        {
            "all_expr_u8": [
                "-010",
                "-0001",
                "0000",
                "001",
                "000010",
                None,
                None,
                "+001",
            ],
            "all_expr": ["-010", "-0001", "0000", "001", "000010", None, None, "+001"],
            "str_lit": ["010", "0010", "010", "10", "00010", "010", None, "010"],
            "len_lit": [
                "-0010",
                "-0001",
                "00000",
                "00001",
                "00010",
                None,
                "00001",
                "+0001",
            ],
        }
    )
    assert_frame_equal(out, expected)


def test_str_zfill_wrong_length() -> None:
    df = pl.DataFrame({"num": ["-10", "-1", "0"]})
    with pytest.raises(ShapeError):
        df.select(pl.col("num").str.zfill(pl.Series([1, 2])))


def test_pad_end_unicode() -> None:
    lf = pl.LazyFrame({"a": ["Café", "345", "東京", None]})

    result = lf.select(pl.col("a").str.pad_end(6, "日"))

    expected = pl.LazyFrame({"a": ["Café日日", "345日日日", "東京日日日日", None]})
    assert_frame_equal(result, expected)


def test_pad_start_unicode() -> None:
    lf = pl.LazyFrame({"a": ["Café", "345", "東京", None]})

    result = lf.select(pl.col("a").str.pad_start(6, "日"))

    expected = pl.LazyFrame({"a": ["日日Café", "日日日345", "日日日日東京", None]})
    assert_frame_equal(result, expected)


def test_str_zfill_unicode_not_respected() -> None:
    lf = pl.LazyFrame({"a": ["Café", "345", "東京", None]})

    result = lf.select(pl.col("a").str.zfill(6))

    expected = pl.LazyFrame({"a": ["0Café", "000345", "東京", None]})
    assert_frame_equal(result, expected)
