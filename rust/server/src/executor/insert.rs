use std::sync::Arc;

use protobuf::Message;

use pb_convert::FromProtobuf;
use risingwave_proto::plan::{InsertNode, PlanNode_PlanNodeType};

use crate::executor::ExecutorResult::{Batch, Done};
use crate::executor::{BoxedExecutorBuilder, Executor, ExecutorBuilder, ExecutorResult};
use crate::storage::*;
use risingwave_common::array::column::Column;
use risingwave_common::array::{ArrayBuilder, DataChunk, PrimitiveArrayBuilder};
use risingwave_common::catalog::TableId;
use risingwave_common::catalog::{Field, Schema};
use risingwave_common::error::ErrorCode::{InternalError, ProtobufError};
use risingwave_common::error::{ErrorCode, Result, RwError};
use risingwave_common::types::Int32Type;

use super::BoxedExecutor;

/// `InsertExecutor` implements table insertion with values from its child executor.
pub(super) struct InsertExecutor {
    /// target table id
    table_id: TableId,
    table_manager: TableManagerRef,

    child: BoxedExecutor,
    executed: bool,
    schema: Schema,
}

#[async_trait::async_trait]
impl Executor for InsertExecutor {
    fn init(&mut self) -> Result<()> {
        self.child.init()?;
        info!("Insert executor");
        Ok(())
    }

    async fn execute(&mut self) -> Result<ExecutorResult> {
        if self.executed {
            return Ok(Done);
        }

        let table_ref = (if let SimpleTableRef::Columnar(table_ref) =
            self.table_manager.get_table(&self.table_id)?
        {
            Ok(table_ref)
        } else {
            Err(RwError::from(InternalError(
                "Only columnar table support insert".to_string(),
            )))
        })?;
        let mut rows_inserted = 0;
        while let Batch(child_chunk) = self.child.execute().await? {
            rows_inserted += table_ref.append(child_chunk).await?;
        }

        // create ret value
        {
            let mut array_builder = PrimitiveArrayBuilder::<i32>::new(1)?;
            array_builder.append(Some(rows_inserted as i32))?;

            let array = array_builder.finish()?;
            let ret_chunk = DataChunk::builder()
                .columns(vec![Column::new(
                    Arc::new(array.into()),
                    Int32Type::create(false),
                )])
                .build();

            self.executed = true;
            Ok(ExecutorResult::Batch(ret_chunk))
        }
    }

    fn clean(&mut self) -> Result<()> {
        self.child.clean()?;
        info!("Cleaning insert executor.");
        Ok(())
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }
}

#[async_trait::async_trait]
impl BoxedExecutorBuilder for InsertExecutor {
    async fn new_boxed_executor(source: &ExecutorBuilder) -> Result<BoxedExecutor> {
        ensure!(source.plan_node().get_node_type() == PlanNode_PlanNodeType::INSERT);
        let insert_node = InsertNode::parse_from_bytes(source.plan_node().get_body().get_value())
            .map_err(ProtobufError)?;

        let table_id = TableId::from_protobuf(insert_node.get_table_ref_id())
            .map_err(|e| InternalError(format!("Failed to parse table id: {:?}", e)))?;

        let table_manager = source.global_task_env().table_manager_ref();

        let proto_child = source.plan_node.get_children().get(0).ok_or_else(|| {
            RwError::from(ErrorCode::InternalError(String::from(
                "Child interpreting error",
            )))
        })?;
        let child = source.clone_for_plan(proto_child).build().await?;

        Ok(Box::new(Self {
            table_id,
            table_manager,
            child,
            executed: false,
            schema: Schema {
                fields: vec![Field {
                    data_type: Int32Type::create(false),
                }],
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pb_construct::make_proto;
    use risingwave_proto::data::{DataType as DataTypeProto, DataType_TypeName};
    use risingwave_proto::plan::{ColumnDesc, ColumnDesc_ColumnEncodingType};

    use crate::executor::test_utils::MockExecutor;
    use crate::storage::{SimpleTableManager, TableManager};
    use crate::*;
    use risingwave_common::array::{Array, I64Array};
    use risingwave_common::catalog::test_utils::mock_table_id;
    use risingwave_common::catalog::{Field, Schema};
    use risingwave_common::column_nonnull;
    use risingwave_common::types::{DataTypeKind, Int64Type};

    use super::*;

    #[tokio::test]
    async fn test_insert_executor() -> Result<()> {
        let table_id = mock_table_id();
        let table_manager = Arc::new(SimpleTableManager::new());
        let schema = Schema {
            fields: vec![
                Field {
                    data_type: Int64Type::create(false),
                },
                Field {
                    data_type: Int64Type::create(false),
                },
            ],
        };
        let mut mock_executor = MockExecutor::new(schema);
        let column1 = make_proto!(ColumnDesc, {
          column_type: make_proto!(DataTypeProto, {
            type_name: DataType_TypeName::INT64
          }),
          encoding: ColumnDesc_ColumnEncodingType::RAW,
          is_primary: false,
          name: "test_col".to_string()
        });
        let column2 = make_proto!(ColumnDesc, {
          column_type: make_proto!(DataTypeProto, {
            type_name: DataType_TypeName::INT64
          }),
          encoding: ColumnDesc_ColumnEncodingType::RAW,
          is_primary: false,
          name: "test_col".to_string()
        });
        let columns = vec![column1, column2];

        table_manager.create_table(&table_id, &columns)?;
        let col1 = column_nonnull! { I64Array, Int64Type, [1, 3, 5, 7, 9] };
        let col2 = column_nonnull! { I64Array, Int64Type, [2, 4, 6, 8, 10] };
        let data_chunk = DataChunk::builder().columns(vec![col1, col2]).build();
        mock_executor.add(data_chunk);

        let mut insert_executor = InsertExecutor {
            table_id,
            table_manager,
            child: Box::new(mock_executor),
            executed: false,
            schema: Schema {
                fields: vec![Field {
                    data_type: Int32Type::create(false),
                }],
            },
        };
        assert!(insert_executor.init().is_ok());

        let fields = &insert_executor.schema().fields;
        assert_eq!(fields[0].data_type.data_type_kind(), DataTypeKind::Int32);

        let result = insert_executor.execute().await?.batch_or()?;
        assert!(insert_executor.clean().is_ok());
        assert_eq!(
            result
                .column_at(0)?
                .array()
                .as_int32()
                .iter()
                .collect::<Vec<_>>(),
            vec![Some(5)]
        );

        let table_ref = insert_executor
            .table_manager
            .get_table(&insert_executor.table_id)?;
        if let SimpleTableRef::Columnar(table_ref) = table_ref {
            let data_ref = table_ref.get_data().await?;
            assert_eq!(
                data_ref[0]
                    .column_at(0)?
                    .array()
                    .as_int64()
                    .iter()
                    .collect::<Vec<_>>(),
                vec![Some(1), Some(3), Some(5), Some(7), Some(9)]
            );
            assert_eq!(
                data_ref[0]
                    .column_at(1)?
                    .array()
                    .as_int64()
                    .iter()
                    .collect::<Vec<_>>(),
                vec![Some(2), Some(4), Some(6), Some(8), Some(10)]
            );
        } else {
            panic!("invalid table type found.")
        }

        Ok(())
    }
}
