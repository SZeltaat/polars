//! This module exists to reduce compilation times.
//!
//! All the data types are backed by a physical type in memory e.g. Date -> i32, Datetime-> i64.
//!
//! Series lead to code implementations of all traits. Whereas there are a lot of duplicates due to
//! data types being backed by the same physical type. In this module we reduce compile times by
//! opting for a little more run time cost. We cast to the physical type -> apply the operation and
//! (depending on the result) cast back to the original type
//!
use super::*;
#[cfg(feature = "algorithm_group_by")]
use crate::frame::group_by::*;
use crate::prelude::*;

unsafe impl IntoSeries for TimeChunked {
    fn into_series(self) -> Series {
        Series(Arc::new(SeriesWrap(self)))
    }
}

impl private::PrivateSeries for SeriesWrap<TimeChunked> {
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
        let other = other.to_physical_repr().into_owned();
        self.0
            .physical()
            .zip_with(mask, other.as_ref().as_ref())
            .map(|ca| ca.into_time().into_series())
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
    unsafe fn agg_min(&self, groups: &GroupsType) -> Series {
        self.0.physical().agg_min(groups).into_time().into_series()
    }

    #[cfg(feature = "algorithm_group_by")]
    unsafe fn agg_max(&self, groups: &GroupsType) -> Series {
        self.0.physical().agg_max(groups).into_time().into_series()
    }

    #[cfg(feature = "algorithm_group_by")]
    unsafe fn agg_list(&self, groups: &GroupsType) -> Series {
        // we cannot cast and dispatch as the inner type of the list would be incorrect
        self.0
            .physical()
            .agg_list(groups)
            .cast(&DataType::List(Box::new(self.dtype().clone())))
            .unwrap()
    }

    fn subtract(&self, rhs: &Series) -> PolarsResult<Series> {
        let rhs = rhs.time().map_err(|_| polars_err!(InvalidOperation: "cannot subtract a {} dtype with a series of type: {}", self.dtype(), rhs.dtype()))?;

        let phys = self
            .0
            .physical()
            .subtract(&rhs.physical().clone().into_series())?;

        Ok(phys.into_duration(TimeUnit::Nanoseconds))
    }

    fn add_to(&self, rhs: &Series) -> PolarsResult<Series> {
        polars_bail!(opq = add, DataType::Time, rhs.dtype());
    }

    fn multiply(&self, rhs: &Series) -> PolarsResult<Series> {
        polars_bail!(opq = mul, self.0.dtype(), rhs.dtype());
    }

    fn divide(&self, rhs: &Series) -> PolarsResult<Series> {
        polars_bail!(opq = div, self.0.dtype(), rhs.dtype());
    }

    fn remainder(&self, rhs: &Series) -> PolarsResult<Series> {
        polars_bail!(opq = rem, self.0.dtype(), rhs.dtype());
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

impl SeriesTrait for SeriesWrap<TimeChunked> {
    fn rename(&mut self, name: PlSmallStr) {
        self.0.rename(name);
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

    fn shrink_to_fit(&mut self) {
        self.0.physical_mut().shrink_to_fit()
    }

    fn slice(&self, offset: i64, length: usize) -> Series {
        self.0.slice(offset, length).into_series()
    }
    fn split_at(&self, offset: i64) -> (Series, Series) {
        let (a, b) = self.0.split_at(offset);
        (a.into_series(), b.into_series())
    }

    fn _sum_as_f64(&self) -> f64 {
        self.0.physical()._sum_as_f64()
    }

    fn mean(&self) -> Option<f64> {
        self.0.physical().mean()
    }

    fn median(&self) -> Option<f64> {
        self.0.physical().median()
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
                .downcast_mut::<TimeChunked>()
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
        self.0
            .physical()
            .filter(filter)
            .map(|ca| ca.into_time().into_series())
    }

    fn take(&self, indices: &IdxCa) -> PolarsResult<Series> {
        Ok(self.0.physical().take(indices)?.into_time().into_series())
    }

    unsafe fn take_unchecked(&self, indices: &IdxCa) -> Series {
        self.0
            .physical()
            .take_unchecked(indices)
            .into_time()
            .into_series()
    }

    fn take_slice(&self, indices: &[IdxSize]) -> PolarsResult<Series> {
        Ok(self.0.physical().take(indices)?.into_time().into_series())
    }

    unsafe fn take_slice_unchecked(&self, indices: &[IdxSize]) -> Series {
        self.0
            .physical()
            .take_unchecked(indices)
            .into_time()
            .into_series()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn rechunk(&self) -> Series {
        self.0
            .physical()
            .rechunk()
            .into_owned()
            .into_time()
            .into_series()
    }

    fn new_from_index(&self, index: usize, length: usize) -> Series {
        self.0
            .physical()
            .new_from_index(index, length)
            .into_time()
            .into_series()
    }

    fn cast(&self, dtype: &DataType, cast_options: CastOptions) -> PolarsResult<Series> {
        match dtype {
            DataType::String => Ok(self
                .0
                .clone()
                .into_series()
                .time()
                .unwrap()
                .to_string("%T")
                .into_series()),
            _ => self.0.cast_with_options(dtype, cast_options),
        }
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
            .into_time()
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
        self.0
            .physical()
            .unique()
            .map(|ca| ca.into_time().into_series())
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
        self.0.physical().reverse().into_time().into_series()
    }

    fn as_single_ptr(&mut self) -> PolarsResult<usize> {
        self.0.physical_mut().as_single_ptr()
    }

    fn shift(&self, periods: i64) -> Series {
        self.0.physical().shift(periods).into_time().into_series()
    }

    fn max_reduce(&self) -> PolarsResult<Scalar> {
        let sc = self.0.physical().max_reduce();
        let av = sc.value().cast(self.dtype()).into_static();
        Ok(Scalar::new(self.dtype().clone(), av))
    }

    fn min_reduce(&self) -> PolarsResult<Scalar> {
        let sc = self.0.physical().min_reduce();
        let av = sc.value().cast(self.dtype()).into_static();
        Ok(Scalar::new(self.dtype().clone(), av))
    }

    fn median_reduce(&self) -> PolarsResult<Scalar> {
        let av = AnyValue::from(self.median().map(|v| v as i64))
            .cast(self.dtype())
            .into_static();
        Ok(Scalar::new(self.dtype().clone(), av))
    }

    fn clone_inner(&self) -> Arc<dyn SeriesTrait> {
        Arc::new(SeriesWrap(Clone::clone(&self.0)))
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

impl private::PrivateSeriesNumeric for SeriesWrap<TimeChunked> {
    fn bit_repr(&self) -> Option<BitRepr> {
        Some(self.0.physical().to_bit_repr())
    }
}
