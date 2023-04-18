// Copyright 2022 The Blaze Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::convert::TryInto;
use std::fmt::Debug;
use std::{any::Any, sync::Arc};
use arrow::array::*;
use arrow::compute::*;
use arrow::datatypes::*;
use arrow::record_batch::RecordBatch;
use datafusion::common::cast::{as_list_array, as_struct_array};
use datafusion::common::DataFusionError;
use datafusion::common::Result;
use datafusion::common::ScalarValue;
use datafusion::logical_expr::ColumnarValue;
use datafusion::logical_expr::field_util::get_indexed_field;
use datafusion::physical_expr::PhysicalExpr;

use crate::down_cast_any_ref;

/// expression to get a field of a list array.
#[derive(Debug)]
pub struct GetIndexedFieldExpr {
    arg: Arc<dyn PhysicalExpr>,
    key: ScalarValue,
}

impl GetIndexedFieldExpr {
    /// Create new get field expression
    pub fn new(arg: Arc<dyn PhysicalExpr>, key: ScalarValue) -> Self {
        Self { arg, key }
    }

    /// Get the input key
    pub fn key(&self) -> &ScalarValue {
        &self.key
    }

    /// Get the input expression
    pub fn arg(&self) -> &Arc<dyn PhysicalExpr> {
        &self.arg
    }
}

impl std::fmt::Display for GetIndexedFieldExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "({}).[{}]", self.arg, self.key)
    }
}

impl PhysicalExpr for GetIndexedFieldExpr {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self, input_schema: &Schema) -> Result<DataType> {
        let data_type = self.arg.data_type(input_schema)?;
        get_indexed_field(&data_type, &self.key).map(|f| f.data_type().clone())
    }

    fn nullable(&self, input_schema: &Schema) -> Result<bool> {
        let data_type = self.arg.data_type(input_schema)?;
        get_indexed_field(&data_type, &self.key).map(|f| f.is_nullable())
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let array = self.arg.evaluate(batch)?.into_array(1);
        match (array.data_type(), &self.key) {
            (DataType::List(_) | DataType::Struct(_), _) if self.key.is_null() => {
                let scalar_null: ScalarValue = array.data_type().try_into()?;
                Ok(ColumnarValue::Scalar(scalar_null))
            }
            (DataType::List(lst), &ScalarValue::Int64(Some(idx))) => {
                let as_list_array = as_list_array(&array)?;

                if idx < 1 || as_list_array.is_empty() {
                    let scalar_null: ScalarValue = lst.data_type().try_into()?;
                    return Ok(ColumnarValue::Scalar(scalar_null))
                }

                let list_len = as_list_array.len();
                let mut take_indices_builder = Int32Builder::with_capacity(list_len);
                for (i, array) in as_list_array.iter().enumerate() {
                    match array {
                        Some(array) if idx <= array.len() as i64 => {
                            let base_offset = as_list_array.value_offsets()[i];
                            let take_offset = base_offset + idx as i32 - 1;
                            take_indices_builder.append_value(take_offset);
                        }
                        _ => {
                            take_indices_builder.append_null()
                        }
                    }
                }
                let taken = take(
                    &as_list_array.values(),
                    &take_indices_builder.finish(),
                    None,
                )?;
                Ok(ColumnarValue::Array(taken))
            }
            (DataType::Struct(_), ScalarValue::Utf8(Some(k))) => {
                let as_struct_array = as_struct_array(&array)?;
                match as_struct_array.column_by_name(k) {
                    None => Err(DataFusionError::Execution(
                        format!("get indexed field {k} not found in struct"))),
                    Some(col) => Ok(ColumnarValue::Array(col.clone()))
                }
            }
            (DataType::List(_), key) => Err(DataFusionError::Execution(
                format!("get indexed field is only possible on lists with int64 indexes. \
                         Tried with {key:?} index"))),
            (DataType::Struct(_), key) => Err(DataFusionError::Execution(
                format!("get indexed field is only possible on struct with utf8 indexes. \
                         Tried with {key:?} index"))),
            (dt, key) => Err(DataFusionError::Execution(
                format!("get indexed field is only possible on lists with int64 indexes or struct \
                         with utf8 indexes. Tried {dt:?} with {key:?} index"))),
        }
    }

    fn children(&self) -> Vec<Arc<dyn PhysicalExpr>> {
        vec![self.arg.clone()]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        Ok(Arc::new(GetIndexedFieldExpr::new(
            children[0].clone(),
            self.key.clone(),
        )))
    }
}

impl PartialEq<dyn Any> for GetIndexedFieldExpr {
    fn eq(&self, other: &dyn Any) -> bool {
        down_cast_any_ref(other)
            .downcast_ref::<Self>()
            .map(|x| self.arg.eq(&x.arg) && self.key == x.key)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use arrow::array::*;
    use arrow::compute::*;
    use arrow::datatypes::*;
    use datafusion::physical_plan::PhysicalExpr;
    use datafusion::physical_plan::expressions::Column;
    use datafusion::scalar::ScalarValue;
    use arrow::record_batch::RecordBatch;
    use datafusion::assert_batches_eq;
    use super::GetIndexedFieldExpr;

    #[test]
    fn test_list() -> Result<(), Box<dyn std::error::Error>> {
        let array: ArrayRef = Arc::new(
            ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
                Some(vec![Some(100), Some(101), Some(102)]),
                Some(vec![Some(200), Some(201)]),
                None,
                Some(vec![Some(300)]),
                Some(vec![Some(400), Some(401), None, Some(403)]),
            ]));
        let input_batch = RecordBatch::try_from_iter_with_nullable(vec![
            ("c1", array, true),
        ])?;

        let get_indexed = Arc::new(GetIndexedFieldExpr::new(
            Arc::new(Column::new("c1", 0)),
            ScalarValue::from(2_i64),
        ));
        let output_array = get_indexed.evaluate(&input_batch)?.into_array(0);
        let output_batch = RecordBatch::try_from_iter_with_nullable(vec![
            ("c1", output_array, true),
        ])?;

        let expected = vec![
            "+-----+",
            "| c1  |",
            "+-----+",
            "| 101 |",
            "| 201 |",
            "|     |",
            "|     |",
            "| 401 |",
            "+-----+",
        ];
        assert_batches_eq!(expected, &vec![output_batch]);
        Ok(())
    }
}