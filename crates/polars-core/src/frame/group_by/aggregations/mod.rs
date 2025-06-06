mod agg_list;
mod boolean;
mod dispatch;
mod string;

use std::borrow::Cow;

pub use agg_list::*;
use arrow::bitmap::{Bitmap, MutableBitmap};
use arrow::legacy::kernels::take_agg::*;
use arrow::legacy::trusted_len::TrustedLenPush;
use arrow::types::NativeType;
use num_traits::pow::Pow;
use num_traits::{Bounded, Float, Num, NumCast, ToPrimitive, Zero};
use polars_compute::rolling::no_nulls::{
    MaxWindow, MeanWindow, MinWindow, MomentWindow, QuantileWindow, RollingAggWindowNoNulls,
    SumWindow,
};
use polars_compute::rolling::nulls::{RollingAggWindowNulls, VarianceMoment};
use polars_compute::rolling::quantile_filter::SealedRolling;
use polars_compute::rolling::{
    self, QuantileMethod, RollingFnParams, RollingQuantileParams, RollingVarParams, quantile_filter,
};
use polars_utils::float::IsFloat;
use polars_utils::idx_vec::IdxVec;
use polars_utils::min_max::MinMax;
use rayon::prelude::*;

use crate::chunked_array::cast::CastOptions;
#[cfg(feature = "object")]
use crate::chunked_array::object::extension::create_extension;
use crate::frame::group_by::GroupsIdx;
#[cfg(feature = "object")]
use crate::frame::group_by::GroupsIndicator;
use crate::prelude::*;
use crate::series::IsSorted;
use crate::series::implementations::SeriesWrap;
use crate::utils::NoNull;
use crate::{POOL, apply_method_physical_integer};

fn idx2usize(idx: &[IdxSize]) -> impl ExactSizeIterator<Item = usize> + '_ {
    idx.iter().map(|i| *i as usize)
}

// if the windows overlap, we can use the rolling_<agg> kernels
// they maintain state, which saves a lot of compute by not naively traversing all elements every
// window
//
// if the windows don't overlap, we should not use these kernels as they are single threaded, so
// we miss out on easy parallelization.
pub fn _use_rolling_kernels(groups: &GroupsSlice, chunks: &[ArrayRef]) -> bool {
    match groups.len() {
        0 | 1 => false,
        _ => {
            let [first_offset, first_len] = groups[0];
            let second_offset = groups[1][0];

            second_offset >= first_offset // Prevent false positive from regular group-by that has out of order slices.
                                          // Rolling group-by is expected to have monotonically increasing slices.
                && second_offset < (first_offset + first_len)
                && chunks.len() == 1
        },
    }
}

// Use an aggregation window that maintains the state
pub fn _rolling_apply_agg_window_nulls<'a, Agg, T, O>(
    values: &'a [T],
    validity: &'a Bitmap,
    offsets: O,
    params: Option<RollingFnParams>,
) -> PrimitiveArray<T>
where
    O: Iterator<Item = (IdxSize, IdxSize)> + TrustedLen,
    Agg: RollingAggWindowNulls<'a, T>,
    T: IsFloat + NativeType,
{
    if values.is_empty() {
        let out: Vec<T> = vec![];
        return PrimitiveArray::new(T::PRIMITIVE.into(), out.into(), None);
    }

    // This iterators length can be trusted
    // these represent the number of groups in the group_by operation
    let output_len = offsets.size_hint().0;
    // start with a dummy index, will be overwritten on first iteration.
    // SAFETY:
    // we are in bounds
    let mut agg_window = unsafe { Agg::new(values, validity, 0, 0, params, None) };

    let mut validity = MutableBitmap::with_capacity(output_len);
    validity.extend_constant(output_len, true);

    let out = offsets
        .enumerate()
        .map(|(idx, (start, len))| {
            let end = start + len;

            // SAFETY:
            // we are in bounds

            let agg = if start == end {
                None
            } else {
                unsafe { agg_window.update(start as usize, end as usize) }
            };

            match agg {
                Some(val) => val,
                None => {
                    // SAFETY: we are in bounds
                    unsafe { validity.set_unchecked(idx, false) };
                    T::default()
                },
            }
        })
        .collect_trusted::<Vec<_>>();

    PrimitiveArray::new(T::PRIMITIVE.into(), out.into(), Some(validity.into()))
}

// Use an aggregation window that maintains the state.
pub fn _rolling_apply_agg_window_no_nulls<'a, Agg, T, O>(
    values: &'a [T],
    offsets: O,
    params: Option<RollingFnParams>,
) -> PrimitiveArray<T>
where
    // items (offset, len) -> so offsets are offset, offset + len
    Agg: RollingAggWindowNoNulls<'a, T>,
    O: Iterator<Item = (IdxSize, IdxSize)> + TrustedLen,
    T: IsFloat + NativeType,
{
    if values.is_empty() {
        let out: Vec<T> = vec![];
        return PrimitiveArray::new(T::PRIMITIVE.into(), out.into(), None);
    }
    // start with a dummy index, will be overwritten on first iteration.
    let mut agg_window = Agg::new(values, 0, 0, params, None);

    offsets
        .map(|(start, len)| {
            let end = start + len;

            if start == end {
                None
            } else {
                // SAFETY: we are in bounds.
                unsafe { agg_window.update(start as usize, end as usize) }
            }
        })
        .collect::<PrimitiveArray<T>>()
}

pub fn _slice_from_offsets<T>(ca: &ChunkedArray<T>, first: IdxSize, len: IdxSize) -> ChunkedArray<T>
where
    T: PolarsDataType,
{
    ca.slice(first as i64, len as usize)
}

/// Helper that combines the groups into a parallel iterator over `(first, all): (u32, &Vec<u32>)`.
pub fn _agg_helper_idx<T, F>(groups: &GroupsIdx, f: F) -> Series
where
    F: Fn((IdxSize, &IdxVec)) -> Option<T::Native> + Send + Sync,
    T: PolarsNumericType,
{
    let ca: ChunkedArray<T> = POOL.install(|| groups.into_par_iter().map(f).collect());
    ca.into_series()
}

/// Same helper as `_agg_helper_idx` but for aggregations that don't return an Option.
pub fn _agg_helper_idx_no_null<T, F>(groups: &GroupsIdx, f: F) -> Series
where
    F: Fn((IdxSize, &IdxVec)) -> T::Native + Send + Sync,
    T: PolarsNumericType,
{
    let ca: NoNull<ChunkedArray<T>> = POOL.install(|| groups.into_par_iter().map(f).collect());
    ca.into_inner().into_series()
}

/// Helper that iterates on the `all: Vec<Vec<u32>` collection,
/// this doesn't have traverse the `first: Vec<u32>` memory and is therefore faster.
fn agg_helper_idx_on_all<T, F>(groups: &GroupsIdx, f: F) -> Series
where
    F: Fn(&IdxVec) -> Option<T::Native> + Send + Sync,
    T: PolarsNumericType,
{
    let ca: ChunkedArray<T> = POOL.install(|| groups.all().into_par_iter().map(f).collect());
    ca.into_series()
}

/// Same as `agg_helper_idx_on_all` but for aggregations that don't return an Option.
fn agg_helper_idx_on_all_no_null<T, F>(groups: &GroupsIdx, f: F) -> Series
where
    F: Fn(&IdxVec) -> T::Native + Send + Sync,
    T: PolarsNumericType,
{
    let ca: NoNull<ChunkedArray<T>> =
        POOL.install(|| groups.all().into_par_iter().map(f).collect());
    ca.into_inner().into_series()
}

pub fn _agg_helper_slice<T, F>(groups: &[[IdxSize; 2]], f: F) -> Series
where
    F: Fn([IdxSize; 2]) -> Option<T::Native> + Send + Sync,
    T: PolarsNumericType,
{
    let ca: ChunkedArray<T> = POOL.install(|| groups.par_iter().copied().map(f).collect());
    ca.into_series()
}

pub fn _agg_helper_slice_no_null<T, F>(groups: &[[IdxSize; 2]], f: F) -> Series
where
    F: Fn([IdxSize; 2]) -> T::Native + Send + Sync,
    T: PolarsNumericType,
{
    let ca: NoNull<ChunkedArray<T>> = POOL.install(|| groups.par_iter().copied().map(f).collect());
    ca.into_inner().into_series()
}

/// Intermediate helper trait so we can have a single generic implementation
/// This trait will ensure the specific dispatch works without complicating
/// the trait bounds.
trait QuantileDispatcher<K> {
    fn _quantile(self, quantile: f64, method: QuantileMethod) -> PolarsResult<Option<K>>;

    fn _median(self) -> Option<K>;
}

impl<T> QuantileDispatcher<f64> for ChunkedArray<T>
where
    T: PolarsIntegerType,
    T::Native: Ord,
{
    fn _quantile(self, quantile: f64, method: QuantileMethod) -> PolarsResult<Option<f64>> {
        self.quantile_faster(quantile, method)
    }
    fn _median(self) -> Option<f64> {
        self.median_faster()
    }
}

impl QuantileDispatcher<f32> for Float32Chunked {
    fn _quantile(self, quantile: f64, method: QuantileMethod) -> PolarsResult<Option<f32>> {
        self.quantile_faster(quantile, method)
    }
    fn _median(self) -> Option<f32> {
        self.median_faster()
    }
}
impl QuantileDispatcher<f64> for Float64Chunked {
    fn _quantile(self, quantile: f64, method: QuantileMethod) -> PolarsResult<Option<f64>> {
        self.quantile_faster(quantile, method)
    }
    fn _median(self) -> Option<f64> {
        self.median_faster()
    }
}

unsafe fn agg_quantile_generic<T, K>(
    ca: &ChunkedArray<T>,
    groups: &GroupsType,
    quantile: f64,
    method: QuantileMethod,
) -> Series
where
    T: PolarsNumericType,
    ChunkedArray<T>: QuantileDispatcher<K::Native>,
    K: PolarsNumericType,
    <K as datatypes::PolarsNumericType>::Native: num_traits::Float + quantile_filter::SealedRolling,
{
    let invalid_quantile = !(0.0..=1.0).contains(&quantile);
    if invalid_quantile {
        return Series::full_null(ca.name().clone(), groups.len(), ca.dtype());
    }
    match groups {
        GroupsType::Idx(groups) => {
            let ca = ca.rechunk();
            agg_helper_idx_on_all::<K, _>(groups, |idx| {
                debug_assert!(idx.len() <= ca.len());
                if idx.is_empty() {
                    return None;
                }
                let take = { ca.take_unchecked(idx) };
                // checked with invalid quantile check
                take._quantile(quantile, method).unwrap_unchecked()
            })
        },
        GroupsType::Slice { groups, .. } => {
            if _use_rolling_kernels(groups, ca.chunks()) {
                // this cast is a no-op for floats
                let s = ca
                    .cast_with_options(&K::get_static_dtype(), CastOptions::Overflowing)
                    .unwrap();
                let ca: &ChunkedArray<K> = s.as_ref().as_ref();
                let arr = ca.downcast_iter().next().unwrap();
                let values = arr.values().as_slice();
                let offset_iter = groups.iter().map(|[first, len]| (*first, *len));
                let arr = match arr.validity() {
                    None => _rolling_apply_agg_window_no_nulls::<QuantileWindow<_>, _, _>(
                        values,
                        offset_iter,
                        Some(RollingFnParams::Quantile(RollingQuantileParams {
                            prob: quantile,
                            method,
                        })),
                    ),
                    Some(validity) => {
                        _rolling_apply_agg_window_nulls::<rolling::nulls::QuantileWindow<_>, _, _>(
                            values,
                            validity,
                            offset_iter,
                            Some(RollingFnParams::Quantile(RollingQuantileParams {
                                prob: quantile,
                                method,
                            })),
                        )
                    },
                };
                // The rolling kernels works on the dtype, this is not yet the
                // float output type we need.
                ChunkedArray::<K>::with_chunk(PlSmallStr::EMPTY, arr).into_series()
            } else {
                _agg_helper_slice::<K, _>(groups, |[first, len]| {
                    debug_assert!(first + len <= ca.len() as IdxSize);
                    match len {
                        0 => None,
                        1 => ca.get(first as usize).map(|v| NumCast::from(v).unwrap()),
                        _ => {
                            let arr_group = _slice_from_offsets(ca, first, len);
                            // unwrap checked with invalid quantile check
                            arr_group
                                ._quantile(quantile, method)
                                .unwrap_unchecked()
                                .map(|flt| NumCast::from(flt).unwrap_unchecked())
                        },
                    }
                })
            }
        },
    }
}

unsafe fn agg_median_generic<T, K>(ca: &ChunkedArray<T>, groups: &GroupsType) -> Series
where
    T: PolarsNumericType,
    ChunkedArray<T>: QuantileDispatcher<K::Native>,
    K: PolarsNumericType,
    <K as datatypes::PolarsNumericType>::Native: num_traits::Float + SealedRolling,
{
    match groups {
        GroupsType::Idx(groups) => {
            let ca = ca.rechunk();
            agg_helper_idx_on_all::<K, _>(groups, |idx| {
                debug_assert!(idx.len() <= ca.len());
                if idx.is_empty() {
                    return None;
                }
                let take = { ca.take_unchecked(idx) };
                take._median()
            })
        },
        GroupsType::Slice { .. } => {
            agg_quantile_generic::<T, K>(ca, groups, 0.5, QuantileMethod::Linear)
        },
    }
}

/// # Safety
///
/// No bounds checks on `groups`.
#[cfg(feature = "bitwise")]
unsafe fn bitwise_agg<T: PolarsNumericType>(
    ca: &ChunkedArray<T>,
    groups: &GroupsType,
    f: fn(&ChunkedArray<T>) -> Option<T::Native>,
) -> Series
where
    ChunkedArray<T>: ChunkTakeUnchecked<[IdxSize]> + ChunkBitwiseReduce<Physical = T::Native>,
{
    // Prevent a rechunk for every individual group.

    let s = if groups.len() > 1 {
        ca.rechunk()
    } else {
        Cow::Borrowed(ca)
    };

    match groups {
        GroupsType::Idx(groups) => agg_helper_idx_on_all::<T, _>(groups, |idx| {
            debug_assert!(idx.len() <= s.len());
            if idx.is_empty() {
                None
            } else {
                let take = unsafe { s.take_unchecked(idx) };
                f(&take)
            }
        }),
        GroupsType::Slice { groups, .. } => _agg_helper_slice::<T, _>(groups, |[first, len]| {
            debug_assert!(len <= s.len() as IdxSize);
            if len == 0 {
                None
            } else {
                let take = _slice_from_offsets(&s, first, len);
                f(&take)
            }
        }),
    }
}

#[cfg(feature = "bitwise")]
impl<T> ChunkedArray<T>
where
    T: PolarsNumericType,
    ChunkedArray<T>: ChunkTakeUnchecked<[IdxSize]> + ChunkBitwiseReduce<Physical = T::Native>,
{
    /// # Safety
    ///
    /// No bounds checks on `groups`.
    pub(crate) unsafe fn agg_and(&self, groups: &GroupsType) -> Series {
        unsafe { bitwise_agg(self, groups, ChunkBitwiseReduce::and_reduce) }
    }

    /// # Safety
    ///
    /// No bounds checks on `groups`.
    pub(crate) unsafe fn agg_or(&self, groups: &GroupsType) -> Series {
        unsafe { bitwise_agg(self, groups, ChunkBitwiseReduce::or_reduce) }
    }

    /// # Safety
    ///
    /// No bounds checks on `groups`.
    pub(crate) unsafe fn agg_xor(&self, groups: &GroupsType) -> Series {
        unsafe { bitwise_agg(self, groups, ChunkBitwiseReduce::xor_reduce) }
    }
}

impl<T> ChunkedArray<T>
where
    T: PolarsNumericType + Sync,
    T::Native: NativeType + PartialOrd + Num + NumCast + Zero + Bounded + std::iter::Sum<T::Native>,
    ChunkedArray<T>: ChunkAgg<T::Native>,
{
    pub(crate) unsafe fn agg_min(&self, groups: &GroupsType) -> Series {
        // faster paths
        match (self.is_sorted_flag(), self.null_count()) {
            (IsSorted::Ascending, 0) => {
                return self.clone().into_series().agg_first(groups);
            },
            (IsSorted::Descending, 0) => {
                return self.clone().into_series().agg_last(groups);
            },
            _ => {},
        }
        match groups {
            GroupsType::Idx(groups) => {
                let ca = self.rechunk();
                let arr = ca.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                _agg_helper_idx::<T, _>(groups, |(first, idx)| {
                    debug_assert!(idx.len() <= arr.len());
                    if idx.is_empty() {
                        None
                    } else if idx.len() == 1 {
                        arr.get(first as usize)
                    } else if no_nulls {
                        take_agg_no_null_primitive_iter_unchecked::<_, T::Native, _, _>(
                            arr,
                            idx2usize(idx),
                            |a, b| a.min_ignore_nan(b),
                        )
                    } else {
                        take_agg_primitive_iter_unchecked(arr, idx2usize(idx), |a, b| {
                            a.min_ignore_nan(b)
                        })
                    }
                })
            },
            GroupsType::Slice {
                groups: groups_slice,
                ..
            } => {
                if _use_rolling_kernels(groups_slice, self.chunks()) {
                    let arr = self.downcast_iter().next().unwrap();
                    let values = arr.values().as_slice();
                    let offset_iter = groups_slice.iter().map(|[first, len]| (*first, *len));
                    let arr = match arr.validity() {
                        None => _rolling_apply_agg_window_no_nulls::<MinWindow<_>, _, _>(
                            values,
                            offset_iter,
                            None,
                        ),
                        Some(validity) => _rolling_apply_agg_window_nulls::<
                            rolling::nulls::MinWindow<_>,
                            _,
                            _,
                        >(
                            values, validity, offset_iter, None
                        ),
                    };
                    Self::from(arr).into_series()
                } else {
                    _agg_helper_slice::<T, _>(groups_slice, |[first, len]| {
                        debug_assert!(len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => self.get(first as usize),
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                ChunkAgg::min(&arr_group)
                            },
                        }
                    })
                }
            },
        }
    }

    pub(crate) unsafe fn agg_max(&self, groups: &GroupsType) -> Series {
        // faster paths
        match (self.is_sorted_flag(), self.null_count()) {
            (IsSorted::Ascending, 0) => {
                return self.clone().into_series().agg_last(groups);
            },
            (IsSorted::Descending, 0) => {
                return self.clone().into_series().agg_first(groups);
            },
            _ => {},
        }

        match groups {
            GroupsType::Idx(groups) => {
                let ca = self.rechunk();
                let arr = ca.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                _agg_helper_idx::<T, _>(groups, |(first, idx)| {
                    debug_assert!(idx.len() <= arr.len());
                    if idx.is_empty() {
                        None
                    } else if idx.len() == 1 {
                        arr.get(first as usize)
                    } else if no_nulls {
                        take_agg_no_null_primitive_iter_unchecked::<_, T::Native, _, _>(
                            arr,
                            idx2usize(idx),
                            |a, b| a.max_ignore_nan(b),
                        )
                    } else {
                        take_agg_primitive_iter_unchecked(arr, idx2usize(idx), |a, b| {
                            a.max_ignore_nan(b)
                        })
                    }
                })
            },
            GroupsType::Slice {
                groups: groups_slice,
                ..
            } => {
                if _use_rolling_kernels(groups_slice, self.chunks()) {
                    let arr = self.downcast_iter().next().unwrap();
                    let values = arr.values().as_slice();
                    let offset_iter = groups_slice.iter().map(|[first, len]| (*first, *len));
                    let arr = match arr.validity() {
                        None => _rolling_apply_agg_window_no_nulls::<MaxWindow<_>, _, _>(
                            values,
                            offset_iter,
                            None,
                        ),
                        Some(validity) => _rolling_apply_agg_window_nulls::<
                            rolling::nulls::MaxWindow<_>,
                            _,
                            _,
                        >(
                            values, validity, offset_iter, None
                        ),
                    };
                    Self::from(arr).into_series()
                } else {
                    _agg_helper_slice::<T, _>(groups_slice, |[first, len]| {
                        debug_assert!(len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => self.get(first as usize),
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                ChunkAgg::max(&arr_group)
                            },
                        }
                    })
                }
            },
        }
    }

    pub(crate) unsafe fn agg_sum(&self, groups: &GroupsType) -> Series {
        match groups {
            GroupsType::Idx(groups) => {
                let ca = self.rechunk();
                let arr = ca.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                _agg_helper_idx_no_null::<T, _>(groups, |(first, idx)| {
                    debug_assert!(idx.len() <= self.len());
                    if idx.is_empty() {
                        T::Native::zero()
                    } else if idx.len() == 1 {
                        arr.get(first as usize).unwrap_or(T::Native::zero())
                    } else if no_nulls {
                        take_agg_no_null_primitive_iter_unchecked(arr, idx2usize(idx), |a, b| a + b)
                            .unwrap_or(T::Native::zero())
                    } else {
                        take_agg_primitive_iter_unchecked(arr, idx2usize(idx), |a, b| a + b)
                            .unwrap_or(T::Native::zero())
                    }
                })
            },
            GroupsType::Slice { groups, .. } => {
                if _use_rolling_kernels(groups, self.chunks()) {
                    let arr = self.downcast_iter().next().unwrap();
                    let values = arr.values().as_slice();
                    let offset_iter = groups.iter().map(|[first, len]| (*first, *len));
                    let arr = match arr.validity() {
                        None => _rolling_apply_agg_window_no_nulls::<
                            SumWindow<T::Native, T::Native>,
                            _,
                            _,
                        >(values, offset_iter, None),
                        Some(validity) => {
                            _rolling_apply_agg_window_nulls::<
                                rolling::nulls::SumWindow<T::Native, T::Native>,
                                _,
                                _,
                            >(values, validity, offset_iter, None)
                        },
                    };
                    Self::from(arr).into_series()
                } else {
                    _agg_helper_slice_no_null::<T, _>(groups, |[first, len]| {
                        debug_assert!(len <= self.len() as IdxSize);
                        match len {
                            0 => T::Native::zero(),
                            1 => self.get(first as usize).unwrap_or(T::Native::zero()),
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                arr_group.sum().unwrap_or(T::Native::zero())
                            },
                        }
                    })
                }
            },
        }
    }
}

impl<T> SeriesWrap<ChunkedArray<T>>
where
    T: PolarsFloatType,
    ChunkedArray<T>: ChunkVar
        + VarAggSeries
        + ChunkQuantile<T::Native>
        + QuantileAggSeries
        + ChunkAgg<T::Native>,
    T::Native: Pow<T::Native, Output = T::Native>,
{
    pub(crate) unsafe fn agg_mean(&self, groups: &GroupsType) -> Series {
        match groups {
            GroupsType::Idx(groups) => {
                let ca = self.rechunk();
                let arr = ca.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                _agg_helper_idx::<T, _>(groups, |(first, idx)| {
                    // this can fail due to a bug in lazy code.
                    // here users can create filters in aggregations
                    // and thereby creating shorter columns than the original group tuples.
                    // the group tuples are modified, but if that's done incorrect there can be out of bounds
                    // access
                    debug_assert!(idx.len() <= self.len());
                    let out = if idx.is_empty() {
                        None
                    } else if idx.len() == 1 {
                        arr.get(first as usize).map(|sum| sum.to_f64().unwrap())
                    } else if no_nulls {
                        take_agg_no_null_primitive_iter_unchecked::<_, T::Native, _, _>(
                            arr,
                            idx2usize(idx),
                            |a, b| a + b,
                        )
                        .unwrap()
                        .to_f64()
                        .map(|sum| sum / idx.len() as f64)
                    } else {
                        take_agg_primitive_iter_unchecked_count_nulls::<T::Native, _, _, _>(
                            arr,
                            idx2usize(idx),
                            |a, b| a + b,
                            T::Native::zero(),
                            idx.len() as IdxSize,
                        )
                        .map(|(sum, null_count)| {
                            sum.to_f64()
                                .map(|sum| sum / (idx.len() as f64 - null_count as f64))
                                .unwrap()
                        })
                    };
                    out.map(|flt| NumCast::from(flt).unwrap())
                })
            },
            GroupsType::Slice { groups, .. } => {
                if _use_rolling_kernels(groups, self.chunks()) {
                    let arr = self.downcast_iter().next().unwrap();
                    let values = arr.values().as_slice();
                    let offset_iter = groups.iter().map(|[first, len]| (*first, *len));
                    let arr = match arr.validity() {
                        None => _rolling_apply_agg_window_no_nulls::<MeanWindow<_>, _, _>(
                            values,
                            offset_iter,
                            None,
                        ),
                        Some(validity) => _rolling_apply_agg_window_nulls::<
                            rolling::nulls::MeanWindow<_>,
                            _,
                            _,
                        >(
                            values, validity, offset_iter, None
                        ),
                    };
                    ChunkedArray::<T>::from(arr).into_series()
                } else {
                    _agg_helper_slice::<T, _>(groups, |[first, len]| {
                        debug_assert!(len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => self.get(first as usize),
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                arr_group.mean().map(|flt| NumCast::from(flt).unwrap())
                            },
                        }
                    })
                }
            },
        }
    }

    pub(crate) unsafe fn agg_var(&self, groups: &GroupsType, ddof: u8) -> Series
    where
        <T as datatypes::PolarsNumericType>::Native: num_traits::Float,
    {
        let ca = &self.0.rechunk();
        match groups {
            GroupsType::Idx(groups) => {
                let ca = ca.rechunk();
                let arr = ca.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                agg_helper_idx_on_all::<T, _>(groups, |idx| {
                    debug_assert!(idx.len() <= ca.len());
                    if idx.is_empty() {
                        return None;
                    }
                    let out = if no_nulls {
                        take_var_no_null_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    } else {
                        take_var_nulls_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    };
                    out.map(|flt| NumCast::from(flt).unwrap())
                })
            },
            GroupsType::Slice { groups, .. } => {
                if _use_rolling_kernels(groups, self.chunks()) {
                    let arr = self.downcast_iter().next().unwrap();
                    let values = arr.values().as_slice();
                    let offset_iter = groups.iter().map(|[first, len]| (*first, *len));
                    let arr = match arr.validity() {
                        None => _rolling_apply_agg_window_no_nulls::<
                            MomentWindow<_, VarianceMoment>,
                            _,
                            _,
                        >(
                            values,
                            offset_iter,
                            Some(RollingFnParams::Var(RollingVarParams { ddof })),
                        ),
                        Some(validity) => _rolling_apply_agg_window_nulls::<
                            rolling::nulls::MomentWindow<_, VarianceMoment>,
                            _,
                            _,
                        >(
                            values,
                            validity,
                            offset_iter,
                            Some(RollingFnParams::Var(RollingVarParams { ddof })),
                        ),
                    };
                    ChunkedArray::<T>::from(arr).into_series()
                } else {
                    _agg_helper_slice::<T, _>(groups, |[first, len]| {
                        debug_assert!(len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => {
                                if ddof == 0 {
                                    NumCast::from(0)
                                } else {
                                    None
                                }
                            },
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                arr_group.var(ddof).map(|flt| NumCast::from(flt).unwrap())
                            },
                        }
                    })
                }
            },
        }
    }
    pub(crate) unsafe fn agg_std(&self, groups: &GroupsType, ddof: u8) -> Series
    where
        <T as datatypes::PolarsNumericType>::Native: num_traits::Float,
    {
        let ca = &self.0.rechunk();
        match groups {
            GroupsType::Idx(groups) => {
                let arr = ca.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                agg_helper_idx_on_all::<T, _>(groups, |idx| {
                    debug_assert!(idx.len() <= ca.len());
                    if idx.is_empty() {
                        return None;
                    }
                    let out = if no_nulls {
                        take_var_no_null_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    } else {
                        take_var_nulls_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    };
                    out.map(|flt| NumCast::from(flt.sqrt()).unwrap())
                })
            },
            GroupsType::Slice { groups, .. } => {
                if _use_rolling_kernels(groups, self.chunks()) {
                    let arr = ca.downcast_iter().next().unwrap();
                    let values = arr.values().as_slice();
                    let offset_iter = groups.iter().map(|[first, len]| (*first, *len));
                    let arr = match arr.validity() {
                        None => _rolling_apply_agg_window_no_nulls::<
                            MomentWindow<_, VarianceMoment>,
                            _,
                            _,
                        >(
                            values,
                            offset_iter,
                            Some(RollingFnParams::Var(RollingVarParams { ddof })),
                        ),
                        Some(validity) => _rolling_apply_agg_window_nulls::<
                            rolling::nulls::MomentWindow<_, rolling::nulls::VarianceMoment>,
                            _,
                            _,
                        >(
                            values,
                            validity,
                            offset_iter,
                            Some(RollingFnParams::Var(RollingVarParams { ddof })),
                        ),
                    };

                    let mut ca = ChunkedArray::<T>::from(arr);
                    ca.apply_mut(|v| v.powf(NumCast::from(0.5).unwrap()));
                    ca.into_series()
                } else {
                    _agg_helper_slice::<T, _>(groups, |[first, len]| {
                        debug_assert!(len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => {
                                if ddof == 0 {
                                    NumCast::from(0)
                                } else {
                                    None
                                }
                            },
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                arr_group.std(ddof).map(|flt| NumCast::from(flt).unwrap())
                            },
                        }
                    })
                }
            },
        }
    }
}

impl Float32Chunked {
    pub(crate) unsafe fn agg_quantile(
        &self,
        groups: &GroupsType,
        quantile: f64,
        method: QuantileMethod,
    ) -> Series {
        agg_quantile_generic::<_, Float32Type>(self, groups, quantile, method)
    }
    pub(crate) unsafe fn agg_median(&self, groups: &GroupsType) -> Series {
        agg_median_generic::<_, Float32Type>(self, groups)
    }
}
impl Float64Chunked {
    pub(crate) unsafe fn agg_quantile(
        &self,
        groups: &GroupsType,
        quantile: f64,
        method: QuantileMethod,
    ) -> Series {
        agg_quantile_generic::<_, Float64Type>(self, groups, quantile, method)
    }
    pub(crate) unsafe fn agg_median(&self, groups: &GroupsType) -> Series {
        agg_median_generic::<_, Float64Type>(self, groups)
    }
}

impl<T> ChunkedArray<T>
where
    T: PolarsIntegerType,
    ChunkedArray<T>: ChunkAgg<T::Native> + ChunkVar,
    T::Native: NumericNative + Ord,
{
    pub(crate) unsafe fn agg_mean(&self, groups: &GroupsType) -> Series {
        match groups {
            GroupsType::Idx(groups) => {
                let ca = self.rechunk();
                let arr = ca.downcast_get(0).unwrap();
                _agg_helper_idx::<Float64Type, _>(groups, |(first, idx)| {
                    // this can fail due to a bug in lazy code.
                    // here users can create filters in aggregations
                    // and thereby creating shorter columns than the original group tuples.
                    // the group tuples are modified, but if that's done incorrect there can be out of bounds
                    // access
                    debug_assert!(idx.len() <= self.len());
                    if idx.is_empty() {
                        None
                    } else if idx.len() == 1 {
                        self.get(first as usize).map(|sum| sum.to_f64().unwrap())
                    } else {
                        match (self.has_nulls(), self.chunks.len()) {
                            (false, 1) => {
                                take_agg_no_null_primitive_iter_unchecked::<_, f64, _, _>(
                                    arr,
                                    idx2usize(idx),
                                    |a, b| a + b,
                                )
                                .map(|sum| sum / idx.len() as f64)
                            },
                            (_, 1) => {
                                {
                                    take_agg_primitive_iter_unchecked_count_nulls::<
                                        T::Native,
                                        f64,
                                        _,
                                        _,
                                    >(
                                        arr, idx2usize(idx), |a, b| a + b, 0.0, idx.len() as IdxSize
                                    )
                                }
                                .map(|(sum, null_count)| {
                                    sum / (idx.len() as f64 - null_count as f64)
                                })
                            },
                            _ => {
                                let take = { self.take_unchecked(idx) };
                                take.mean()
                            },
                        }
                    }
                })
            },
            GroupsType::Slice {
                groups: groups_slice,
                ..
            } => {
                if _use_rolling_kernels(groups_slice, self.chunks()) {
                    let ca = self
                        .cast_with_options(&DataType::Float64, CastOptions::Overflowing)
                        .unwrap();
                    ca.agg_mean(groups)
                } else {
                    _agg_helper_slice::<Float64Type, _>(groups_slice, |[first, len]| {
                        debug_assert!(first + len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => self.get(first as usize).map(|v| NumCast::from(v).unwrap()),
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                arr_group.mean()
                            },
                        }
                    })
                }
            },
        }
    }

    pub(crate) unsafe fn agg_var(&self, groups: &GroupsType, ddof: u8) -> Series {
        match groups {
            GroupsType::Idx(groups) => {
                let ca_self = self.rechunk();
                let arr = ca_self.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                agg_helper_idx_on_all::<Float64Type, _>(groups, |idx| {
                    debug_assert!(idx.len() <= arr.len());
                    if idx.is_empty() {
                        return None;
                    }
                    if no_nulls {
                        take_var_no_null_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    } else {
                        take_var_nulls_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    }
                })
            },
            GroupsType::Slice {
                groups: groups_slice,
                ..
            } => {
                if _use_rolling_kernels(groups_slice, self.chunks()) {
                    let ca = self
                        .cast_with_options(&DataType::Float64, CastOptions::Overflowing)
                        .unwrap();
                    ca.agg_var(groups, ddof)
                } else {
                    _agg_helper_slice::<Float64Type, _>(groups_slice, |[first, len]| {
                        debug_assert!(first + len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => {
                                if ddof == 0 {
                                    NumCast::from(0)
                                } else {
                                    None
                                }
                            },
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                arr_group.var(ddof)
                            },
                        }
                    })
                }
            },
        }
    }
    pub(crate) unsafe fn agg_std(&self, groups: &GroupsType, ddof: u8) -> Series {
        match groups {
            GroupsType::Idx(groups) => {
                let ca_self = self.rechunk();
                let arr = ca_self.downcast_iter().next().unwrap();
                let no_nulls = arr.null_count() == 0;
                agg_helper_idx_on_all::<Float64Type, _>(groups, |idx| {
                    debug_assert!(idx.len() <= self.len());
                    if idx.is_empty() {
                        return None;
                    }
                    let out = if no_nulls {
                        take_var_no_null_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    } else {
                        take_var_nulls_primitive_iter_unchecked(arr, idx2usize(idx), ddof)
                    };
                    out.map(|v| v.sqrt())
                })
            },
            GroupsType::Slice {
                groups: groups_slice,
                ..
            } => {
                if _use_rolling_kernels(groups_slice, self.chunks()) {
                    let ca = self
                        .cast_with_options(&DataType::Float64, CastOptions::Overflowing)
                        .unwrap();
                    ca.agg_std(groups, ddof)
                } else {
                    _agg_helper_slice::<Float64Type, _>(groups_slice, |[first, len]| {
                        debug_assert!(first + len <= self.len() as IdxSize);
                        match len {
                            0 => None,
                            1 => {
                                if ddof == 0 {
                                    NumCast::from(0)
                                } else {
                                    None
                                }
                            },
                            _ => {
                                let arr_group = _slice_from_offsets(self, first, len);
                                arr_group.std(ddof)
                            },
                        }
                    })
                }
            },
        }
    }

    pub(crate) unsafe fn agg_quantile(
        &self,
        groups: &GroupsType,
        quantile: f64,
        method: QuantileMethod,
    ) -> Series {
        agg_quantile_generic::<_, Float64Type>(self, groups, quantile, method)
    }
    pub(crate) unsafe fn agg_median(&self, groups: &GroupsType) -> Series {
        agg_median_generic::<_, Float64Type>(self, groups)
    }
}
