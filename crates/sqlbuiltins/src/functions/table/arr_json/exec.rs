use std::{any::Any, io::BufReader, sync::Arc};

use async_stream::stream;
use bytes::Buf;
use datafusion::{
    arrow::datatypes::SchemaRef,
    datasource::{
        file_format::file_compression_type::FileCompressionType,
        physical_plan::{FileMeta, FileOpenFuture, FileOpener, FileScanConfig, FileStream},
    },
    error::{DataFusionError, Result},
    execution::TaskContext,
    physical_expr::{OrderingEquivalenceProperties, PhysicalSortExpr},
    physical_plan::{
        metrics::{ExecutionPlanMetricsSet, MetricsSet},
        ordering_equivalence_properties_helper, DisplayAs, DisplayFormatType, ExecutionPlan,
        Partitioning, SendableRecordBatchStream, Statistics,
    },
};
use futures::StreamExt;
use object_store::{GetResultPayload, ObjectStore};
use serde_json::Value;

use super::builder::json_values_to_record_batch;

// TODO add metrics and output ordering
/// Execution plan for scanning array json data source
#[derive(Debug, Clone)]
pub struct ArrayJsonExec {
    base_config: FileScanConfig,
    projected_statistics: Statistics,
    projected_schema: SchemaRef,
    file_compression_type: FileCompressionType,
    metrics: ExecutionPlanMetricsSet,
}

impl ArrayJsonExec {
    pub fn new(base_config: FileScanConfig, file_compression_type: FileCompressionType) -> Self {
        let (projected_schema, projected_statistics, _) = base_config.project();

        Self {
            base_config,
            projected_schema,
            projected_statistics,
            file_compression_type,
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }

    pub fn get_base_config(&self) -> &FileScanConfig {
        &self.base_config
    }
}

impl DisplayAs for ArrayJsonExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "JArraysonExec: ")?;
        self.get_base_config().fmt_as(t, f)
    }
}

impl ExecutionPlan for ArrayJsonExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.projected_schema.clone()
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(self.base_config.file_groups.len())
    }

    fn unbounded_output(&self, _: &[bool]) -> Result<bool> {
        Ok(self.base_config.infinite_source)
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    // TODO replace the slice with the correct implementation
    fn ordering_equivalence_properties(&self) -> OrderingEquivalenceProperties {
        ordering_equivalence_properties_helper(self.schema(), &[])
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        Vec::new()
    }

    fn with_new_children(
        self: Arc<Self>,
        _: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn statistics(&self) -> Statistics {
        self.projected_statistics.clone()
    }

    fn metrics(&self) -> Option<MetricsSet> {
        None
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let batch_size = context.session_config().batch_size();
        let (projected_schema, ..) = self.base_config.project();

        let object_store = context
            .runtime_env()
            .object_store(&self.base_config.object_store_url)?;
        let opener = ArrayJsonOpener::new(
            batch_size,
            projected_schema,
            self.file_compression_type,
            object_store,
        );

        let stream = FileStream::new(&self.base_config, partition, opener, &self.metrics)?;

        Ok(Box::pin(stream) as SendableRecordBatchStream)
    }
}

/// A [`FileOpener`] that opens an Array JSON file and yields a [`FileOpenFuture`]
struct ArrayJsonOpener {
    batch_size: usize,
    projected_schema: SchemaRef,
    file_compression_type: FileCompressionType,
    object_store: Arc<dyn ObjectStore>,
}

impl ArrayJsonOpener {
    /// Returns an  [`ArrayJsonOpener`]
    pub fn new(
        batch_size: usize,
        projected_schema: SchemaRef,
        file_compression_type: FileCompressionType,
        object_store: Arc<dyn ObjectStore>,
    ) -> Self {
        Self {
            batch_size,
            projected_schema,
            file_compression_type,
            object_store,
        }
    }
}

impl FileOpener for ArrayJsonOpener {
    fn open(&self, file_meta: FileMeta) -> Result<FileOpenFuture> {
        let store = self.object_store.clone();
        let schema = self.projected_schema.clone();
        let batch_size = self.batch_size;
        let file_compression_type = self.file_compression_type.to_owned();
        Ok(Box::pin(async move {
            let r = store.get(file_meta.location()).await?;
            let stream = stream! {
                 match r.payload {
                GetResultPayload::File(file, _) => {
                    let decoder = file_compression_type.convert_read(file)?;
                    let mut reader = BufReader::new(decoder);
                    let values: Value = simd_json::serde::from_reader(&mut reader)
                        .map_err(|e| DataFusionError::External(Box::new(e)))?;
                    let rows = json_values_to_record_batch(values.as_array().unwrap(), schema, batch_size);
                        yield(rows);
                }
                GetResultPayload::Stream(_) => {
                      let data = r.bytes().await.map_err(|e| {
                        DataFusionError::External(Box::new(e))
                    })?;
                    let decoder = file_compression_type.convert_read(data.reader())?;
                    let mut reader = BufReader::new(decoder);
                    let values: Value = simd_json::serde::from_reader(&mut reader)
                        .map_err(|e| DataFusionError::External(Box::new(e)))?;
                    let rows = json_values_to_record_batch(values.as_array().unwrap(), schema, batch_size);
                    yield(rows);
                }
            };
            };
            Ok(stream.boxed())
        }))
    }
}
