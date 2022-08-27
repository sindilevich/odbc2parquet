use std::convert::TryInto;

use anyhow::Error;
use atoi::FromRadix10Signed;
use odbc_api::{
    buffers::{AnyColumnView, BufferDescription, BufferKind},
    DataType,
};
use parquet::{
    basic::{ConvertedType, Repetition, Type as PhysicalType},
    column::writer::ColumnWriter,
    data_type::{DataType as _, FixedLenByteArrayType, Int32Type, Int64Type},
    schema::types::Type,
};

use crate::parquet_buffer::ParquetBuffer;

use super::{identical::fetch_decimal_as_identical_with_precision, strategy::ColumnFetchStrategy};

/// Choose how to fetch decimals from ODBC and store them in parquet
pub fn decmial_fetch_strategy(
    is_optional: bool,
    scale: i32,
    precision: usize,
    driver_does_support_i64: bool,
) -> Box<dyn ColumnFetchStrategy> {
    match (precision, scale) {
        (0..=9, 0) => {
            // Values with scale 0 and precision <= 9 can be fetched as i32 from the ODBC and we can
            // use the same physical type to store them in parquet.
            fetch_decimal_as_identical_with_precision::<Int32Type>(is_optional, precision as i32)
        }
        // (0..=9, 1..=9) => {
        //     // As these values have a scale unequal to 0 we read them from the datebase as text, but
        //     // since their precision is <= 9 we will store them as i32 (physical parquet type)

        //     let repetition = if is_optional {
        //         Repetition::OPTIONAL
        //     } else {
        //         Repetition::REQUIRED
        //     };
        //     Box::new(DecimalAsBinary::new(repetition, scale, precision))
        // }
        (10..=18, 0) => {
            // Values with scale 0 and precision <= 18 can be fetched as i64 from the ODBC and we
            // can use the same physical type to store them in parquet. That is, if the database
            // does support fetching values as 64Bit integers.
            if driver_does_support_i64 {
                fetch_decimal_as_identical_with_precision::<Int64Type>(
                    is_optional,
                    precision as i32,
                )
            } else {
                // The database does not support 64Bit integers (looking at you Oracle). So we fetch
                // the values from the database as text and convert them into 64Bit integers.
                Box::new(I64FromText::decimal(is_optional, precision as i32))
            }
        }
        (_, _) => {
            let repetition = if is_optional {
                Repetition::OPTIONAL
            } else {
                Repetition::REQUIRED
            };
            Box::new(DecimalAsBinary::new(repetition, scale, precision))
        }
    }
}

/// Strategy for fetching decimal values which can not be represented as either 32Bit or 64Bit
struct DecimalAsBinary {
    repetition: Repetition,
    scale: i32,
    precision: usize,
    length_in_bytes: usize,
}

impl DecimalAsBinary {
    pub fn new(repetition: Repetition, scale: i32, precision: usize) -> Self {
        // Length of the two's complement.
        let num_binary_digits = precision as f64 * 10f64.log2();
        // Plus one bit for the sign (+/-)
        let length_in_bits = num_binary_digits + 1.0;
        let length_in_bytes = (length_in_bits / 8.0).ceil() as usize;

        Self {
            repetition,
            scale,
            precision,
            length_in_bytes,
        }
    }
}

impl ColumnFetchStrategy for DecimalAsBinary {
    fn parquet_type(&self, name: &str) -> Type {
        Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
            .with_length(self.length_in_bytes.try_into().unwrap())
            .with_converted_type(ConvertedType::DECIMAL)
            .with_precision(self.precision.try_into().unwrap())
            .with_scale(self.scale)
            .with_repetition(self.repetition)
            .build()
            .unwrap()
    }

    fn buffer_description(&self) -> odbc_api::buffers::BufferDescription {
        // Precision + 2. (One byte for the radix character and another for the sign)
        let max_str_len = DataType::Decimal {
            precision: self.precision,
            scale: self.scale.try_into().unwrap(),
        }
        .display_size()
        .unwrap();
        BufferDescription {
            kind: BufferKind::Text { max_str_len },
            nullable: true,
        }
    }

    fn copy_odbc_to_parquet(
        &self,
        parquet_buffer: &mut ParquetBuffer,
        column_writer: &mut ColumnWriter,
        column_view: AnyColumnView,
    ) -> Result<(), Error> {
        write_decimal_col(
            parquet_buffer,
            column_writer,
            column_view,
            self.length_in_bytes,
            self.precision,
        )
    }
}

fn write_decimal_col(
    parquet_buffer: &mut ParquetBuffer,
    column_writer: &mut ColumnWriter,
    column_reader: AnyColumnView,
    length_in_bytes: usize,
    precision: usize,
) -> Result<(), Error> {
    let column_writer = FixedLenByteArrayType::get_column_writer_mut(column_writer).unwrap();
    if let AnyColumnView::Text(view) = column_reader {
        parquet_buffer.write_decimal(column_writer, view.iter(), length_in_bytes, precision)?;
    } else {
        panic!(
            "Invalid Column view type. This is not supposed to happen. Please open a Bug at \
            https://github.com/pacman82/odbc2parquet/issues."
        )
    }
    Ok(())
}

/// Query a column as text and write it as 64 Bit integer.
struct I64FromText {
    /// `true` if NULL is allowed, `false` otherwise
    is_optional: bool,
    /// Maximum total number of digits in the decimal
    precision: i32,
}

impl I64FromText {
    /// Converted type is decimal
    pub fn decimal(is_optional: bool, precision: i32) -> Self {
        Self {
            is_optional,
            precision,
        }
    }
}

impl ColumnFetchStrategy for I64FromText {
    fn parquet_type(&self, name: &str) -> Type {
        let repetition = if self.is_optional {
            Repetition::OPTIONAL
        } else {
            Repetition::REQUIRED
        };
        let physical_type = Int64Type::get_physical_type();

        Type::primitive_type_builder(name, physical_type)
            .with_repetition(repetition)
            .with_converted_type(ConvertedType::DECIMAL)
            .with_precision(self.precision)
            .with_scale(0)
            .build()
            .unwrap()
    }

    fn buffer_description(&self) -> BufferDescription {
        // +1 not for terminating zero, but for the sign charactor like `-` or `+`. Also one
        // additional space for the radix character
        let max_str_len = odbc_api::DataType::Decimal {
            precision: self.precision.try_into().unwrap(),
            scale: 0,
        }
        .display_size()
        .unwrap();
        BufferDescription {
            nullable: self.is_optional,
            kind: BufferKind::Text { max_str_len },
        }
    }

    fn copy_odbc_to_parquet(
        &self,
        parquet_buffer: &mut ParquetBuffer,
        column_writer: &mut ColumnWriter,
        column_view: AnyColumnView,
    ) -> Result<(), Error> {
        let column_writer = Int64Type::get_column_writer_mut(column_writer).unwrap();
        if let AnyColumnView::Text(view) = column_view {
            parquet_buffer.write_optional(
                column_writer,
                view.iter()
                    .map(|value| value.map(|text| i64::from_radix_10_signed(text).0)),
            )?;
        } else {
            panic!(
                "Invalid Column view type. This is not supposed to happen. Please open a Bug at \
                https://github.com/pacman82/odbc2parquet/issues."
            )
        }
        Ok(())
    }
}
