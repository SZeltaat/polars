use polars_compute::rolling::QuantileMethod;

use super::*;
use crate::prelude::*;

unsafe impl IntoSeries for DecimalChunked {
    fn into_series(self) -> Series {
        Series(Arc::new(SeriesWrap(self)))
    }
}

impl private::PrivateSeriesNumeric for SeriesWrap<DecimalChunked> {
    fn bit_repr(&self) -> Option<BitRepr> {
        None
    }
}

impl SeriesWrap<DecimalChunked> {
    fn apply_physical_to_s<F: Fn(&Int128Chunked) -> Int128Chunked>(&self, f: F) -> Series {
        f(self.0.physical())
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series()
    }

    fn apply_physical<T, F: Fn(&Int128Chunked) -> T>(&self, f: F) -> T {
        f(self.0.physical())
    }

    fn scale_factor(&self) -> u128 {
        10u128.pow(self.0.scale() as u32)
    }

    fn apply_scale(&self, mut scalar: Scalar) -> Scalar {
        if scalar.is_null() {
            return scalar;
        }

        debug_assert_eq!(scalar.dtype(), &DataType::Float64);
        let v = scalar
            .value()
            .try_extract::<f64>()
            .expect("should be f64 scalar");
        scalar.update((v / self.scale_factor() as f64).into());
        scalar
    }

    fn agg_helper<F: Fn(&Int128Chunked) -> Series>(&self, f: F) -> Series {
        let agg_s = f(self.0.physical());
        match agg_s.dtype() {
            DataType::Int128 => {
                let ca = agg_s.i128().unwrap();
                let ca = ca.as_ref().clone();
                let precision = self.0.precision();
                let scale = self.0.scale();
                ca.into_decimal_unchecked(precision, scale).into_series()
            },
            DataType::List(dtype) if matches!(dtype.as_ref(), DataType::Int128) => {
                let dtype = self.0.dtype();
                let ca = agg_s.list().unwrap();
                let arr = ca.downcast_iter().next().unwrap();
                // SAFETY: dtype is passed correctly
                let precision = self.0.precision();
                let scale = self.0.scale();
                let s = unsafe {
                    Series::from_chunks_and_dtype_unchecked(
                        PlSmallStr::EMPTY,
                        vec![arr.values().clone()],
                        dtype,
                    )
                }
                .into_decimal(precision, scale)
                .unwrap();
                let new_values = s.array_ref(0).clone();
                let dtype = DataType::Int128;
                let arrow_dtype =
                    ListArray::<i64>::default_datatype(dtype.to_arrow(CompatLevel::newest()));
                let new_arr = ListArray::<i64>::new(
                    arrow_dtype,
                    arr.offsets().clone(),
                    new_values,
                    arr.validity().cloned(),
                );
                unsafe {
                    ListChunked::from_chunks_and_dtype_unchecked(
                        agg_s.name().clone(),
                        vec![Box::new(new_arr)],
                        DataType::List(Box::new(DataType::Decimal(precision, Some(scale)))),
                    )
                    .into_series()
                }
            },
            _ => unreachable!(),
        }
    }
}

impl private::PrivateSeries for SeriesWrap<DecimalChunked> {
    fn compute_len(&mut self) {
        self.0.physical_mut().compute_len()
    }

    fn _field(&self) -> Cow<'_, Field> {
        Cow::Owned(self.0.field())
    }

    fn _dtype(&self) -> &DataType {
        self.0.dtype()
    }
    fn _get_flags(&self) -> StatisticsFlags {
        self.0.physical().get_flags()
    }
    fn _set_flags(&mut self, flags: StatisticsFlags) {
        self.0.physical_mut().set_flags(flags)
    }

    #[cfg(feature = "zip_with")]
    fn zip_with_same_type(&self, mask: &BooleanChunked, other: &Series) -> PolarsResult<Series> {
        let other = other.decimal()?;

        Ok(self
            .0
            .physical()
            .zip_with(mask, other.physical())?
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series())
    }
    fn into_total_eq_inner<'a>(&'a self) -> Box<dyn TotalEqInner + 'a> {
        self.0.physical().into_total_eq_inner()
    }
    fn into_total_ord_inner<'a>(&'a self) -> Box<dyn TotalOrdInner + 'a> {
        self.0.physical().into_total_ord_inner()
    }

    fn vec_hash(
        &self,
        random_state: PlSeedableRandomStateQuality,
        buf: &mut Vec<u64>,
    ) -> PolarsResult<()> {
        self.0.physical().vec_hash(random_state, buf)?;
        Ok(())
    }

    fn vec_hash_combine(
        &self,
        build_hasher: PlSeedableRandomStateQuality,
        hashes: &mut [u64],
    ) -> PolarsResult<()> {
        self.0.physical().vec_hash_combine(build_hasher, hashes)?;
        Ok(())
    }

    #[cfg(feature = "algorithm_group_by")]
    unsafe fn agg_sum(&self, groups: &GroupsType) -> Series {
        self.agg_helper(|ca| ca.agg_sum(groups))
    }

    #[cfg(feature = "algorithm_group_by")]
    unsafe fn agg_min(&self, groups: &GroupsType) -> Series {
        self.agg_helper(|ca| ca.agg_min(groups))
    }

    #[cfg(feature = "algorithm_group_by")]
    unsafe fn agg_max(&self, groups: &GroupsType) -> Series {
        self.agg_helper(|ca| ca.agg_max(groups))
    }

    #[cfg(feature = "algorithm_group_by")]
    unsafe fn agg_list(&self, groups: &GroupsType) -> Series {
        self.agg_helper(|ca| ca.agg_list(groups))
    }

    fn subtract(&self, rhs: &Series) -> PolarsResult<Series> {
        let rhs = rhs.decimal()?;
        ((&self.0) - rhs).map(|ca| ca.into_series())
    }
    fn add_to(&self, rhs: &Series) -> PolarsResult<Series> {
        let rhs = rhs.decimal()?;
        ((&self.0) + rhs).map(|ca| ca.into_series())
    }
    fn multiply(&self, rhs: &Series) -> PolarsResult<Series> {
        let rhs = rhs.decimal()?;
        ((&self.0) * rhs).map(|ca| ca.into_series())
    }
    fn divide(&self, rhs: &Series) -> PolarsResult<Series> {
        let rhs = rhs.decimal()?;
        ((&self.0) / rhs).map(|ca| ca.into_series())
    }
    #[cfg(feature = "algorithm_group_by")]
    fn group_tuples(&self, multithreaded: bool, sorted: bool) -> PolarsResult<GroupsType> {
        self.0.physical().group_tuples(multithreaded, sorted)
    }
    fn arg_sort_multiple(
        &self,
        by: &[Column],
        options: &SortMultipleOptions,
    ) -> PolarsResult<IdxCa> {
        self.0.physical().arg_sort_multiple(by, options)
    }
}

impl SeriesTrait for SeriesWrap<DecimalChunked> {
    fn rename(&mut self, name: PlSmallStr) {
        self.0.rename(name)
    }

    fn chunk_lengths(&self) -> ChunkLenIter<'_> {
        self.0.physical().chunk_lengths()
    }

    fn name(&self) -> &PlSmallStr {
        self.0.name()
    }

    fn chunks(&self) -> &Vec<ArrayRef> {
        self.0.physical().chunks()
    }
    unsafe fn chunks_mut(&mut self) -> &mut Vec<ArrayRef> {
        self.0.physical_mut().chunks_mut()
    }

    fn slice(&self, offset: i64, length: usize) -> Series {
        self.apply_physical_to_s(|ca| ca.slice(offset, length))
    }

    fn split_at(&self, offset: i64) -> (Series, Series) {
        let (a, b) = self.0.split_at(offset);
        (a.into_series(), b.into_series())
    }

    fn append(&mut self, other: &Series) -> PolarsResult<()> {
        polars_ensure!(self.0.dtype() == other.dtype(), append);
        let mut other = other.to_physical_repr().into_owned();
        self.0
            .physical_mut()
            .append_owned(std::mem::take(other._get_inner_mut().as_mut()))
    }
    fn append_owned(&mut self, mut other: Series) -> PolarsResult<()> {
        polars_ensure!(self.0.dtype() == other.dtype(), append);
        self.0.physical_mut().append_owned(std::mem::take(
            &mut other
                ._get_inner_mut()
                .as_any_mut()
                .downcast_mut::<DecimalChunked>()
                .unwrap()
                .phys,
        ))
    }

    fn extend(&mut self, other: &Series) -> PolarsResult<()> {
        polars_ensure!(self.0.dtype() == other.dtype(), extend);
        // 3 refs
        // ref Cow
        // ref SeriesTrait
        // ref ChunkedArray
        let other = other.to_physical_repr();
        self.0
            .physical_mut()
            .extend(other.as_ref().as_ref().as_ref())?;
        Ok(())
    }

    fn filter(&self, filter: &BooleanChunked) -> PolarsResult<Series> {
        Ok(self
            .0
            .physical()
            .filter(filter)?
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series())
    }

    fn take(&self, indices: &IdxCa) -> PolarsResult<Series> {
        Ok(self
            .0
            .physical()
            .take(indices)?
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series())
    }

    unsafe fn take_unchecked(&self, indices: &IdxCa) -> Series {
        self.0
            .physical()
            .take_unchecked(indices)
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series()
    }

    fn take_slice(&self, indices: &[IdxSize]) -> PolarsResult<Series> {
        Ok(self
            .0
            .physical()
            .take(indices)?
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series())
    }

    unsafe fn take_slice_unchecked(&self, indices: &[IdxSize]) -> Series {
        self.0
            .physical()
            .take_unchecked(indices)
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn rechunk(&self) -> Series {
        let ca = self.0.physical().rechunk().into_owned();
        ca.into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series()
    }

    fn new_from_index(&self, index: usize, length: usize) -> Series {
        self.0
            .physical()
            .new_from_index(index, length)
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series()
    }

    fn cast(&self, dtype: &DataType, cast_options: CastOptions) -> PolarsResult<Series> {
        self.0.cast_with_options(dtype, cast_options)
    }

    #[inline]
    unsafe fn get_unchecked(&self, index: usize) -> AnyValue<'_> {
        self.0.get_any_value_unchecked(index)
    }

    fn sort_with(&self, options: SortOptions) -> PolarsResult<Series> {
        Ok(self
            .0
            .physical()
            .sort_with(options)
            .into_decimal_unchecked(self.0.precision(), self.0.scale())
            .into_series())
    }

    fn arg_sort(&self, options: SortOptions) -> IdxCa {
        self.0.physical().arg_sort(options)
    }

    fn null_count(&self) -> usize {
        self.0.null_count()
    }

    fn has_nulls(&self) -> bool {
        self.0.has_nulls()
    }

    #[cfg(feature = "algorithm_group_by")]
    fn unique(&self) -> PolarsResult<Series> {
        Ok(self.apply_physical_to_s(|ca| ca.unique().unwrap()))
    }

    #[cfg(feature = "algorithm_group_by")]
    fn n_unique(&self) -> PolarsResult<usize> {
        self.0.physical().n_unique()
    }

    #[cfg(feature = "algorithm_group_by")]
    fn arg_unique(&self) -> PolarsResult<IdxCa> {
        self.0.physical().arg_unique()
    }

    fn is_null(&self) -> BooleanChunked {
        self.0.is_null()
    }

    fn is_not_null(&self) -> BooleanChunked {
        self.0.is_not_null()
    }

    fn reverse(&self) -> Series {
        self.apply_physical_to_s(|ca| ca.reverse())
    }

    fn shift(&self, periods: i64) -> Series {
        self.apply_physical_to_s(|ca| ca.shift(periods))
    }

    fn clone_inner(&self) -> Arc<dyn SeriesTrait> {
        Arc::new(SeriesWrap(Clone::clone(&self.0)))
    }

    fn sum_reduce(&self) -> PolarsResult<Scalar> {
        Ok(self.apply_physical(|ca| {
            let sum = ca.sum();
            let DataType::Decimal(_, Some(scale)) = self.dtype() else {
                unreachable!()
            };
            let av = AnyValue::Decimal(sum.unwrap(), *scale);
            Scalar::new(self.dtype().clone(), av)
        }))
    }
    fn min_reduce(&self) -> PolarsResult<Scalar> {
        Ok(self.apply_physical(|ca| {
            let min = ca.min();
            let DataType::Decimal(_, Some(scale)) = self.dtype() else {
                unreachable!()
            };
            let av = if let Some(min) = min {
                AnyValue::Decimal(min, *scale)
            } else {
                AnyValue::Null
            };
            Scalar::new(self.dtype().clone(), av)
        }))
    }
    fn max_reduce(&self) -> PolarsResult<Scalar> {
        Ok(self.apply_physical(|ca| {
            let max = ca.max();
            let DataType::Decimal(_, Some(scale)) = self.dtype() else {
                unreachable!()
            };
            let av = if let Some(m) = max {
                AnyValue::Decimal(m, *scale)
            } else {
                AnyValue::Null
            };
            Scalar::new(self.dtype().clone(), av)
        }))
    }

    fn _sum_as_f64(&self) -> f64 {
        self.0.physical()._sum_as_f64() / self.scale_factor() as f64
    }

    fn mean(&self) -> Option<f64> {
        self.0
            .physical()
            .mean()
            .map(|v| v / self.scale_factor() as f64)
    }

    fn median(&self) -> Option<f64> {
        self.0
            .physical()
            .median()
            .map(|v| v / self.scale_factor() as f64)
    }
    fn median_reduce(&self) -> PolarsResult<Scalar> {
        Ok(self.apply_scale(self.0.physical().median_reduce()))
    }

    fn std(&self, ddof: u8) -> Option<f64> {
        self.0
            .physical()
            .std(ddof)
            .map(|v| v / self.scale_factor() as f64)
    }
    fn std_reduce(&self, ddof: u8) -> PolarsResult<Scalar> {
        Ok(self.apply_scale(self.0.physical().std_reduce(ddof)))
    }

    fn quantile_reduce(&self, quantile: f64, method: QuantileMethod) -> PolarsResult<Scalar> {
        self.0
            .physical()
            .quantile_reduce(quantile, method)
            .map(|v| self.apply_scale(v))
    }

    fn find_validity_mismatch(&self, other: &Series, idxs: &mut Vec<IdxSize>) {
        self.0.physical().find_validity_mismatch(other, idxs)
    }

    fn as_any(&self) -> &dyn Any {
        &self.0
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        &mut self.0
    }

    fn as_phys_any(&self) -> &dyn Any {
        self.0.physical()
    }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self as _
    }
}
