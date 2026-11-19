//! DuckDB's C API, loaded at runtime from the downloaded `libduckdb` (see `install.rs`): the app
//! doesn't link DuckDB, so it stays small for everyone who doesn't use it.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void};

pub type idx_t = u64;
pub type duckdb_state = u32;
pub const SUCCESS: duckdb_state = 0;

/// Opaque handles (`typedef struct _x { void *internal_ptr; } *x`).
pub type duckdb_instance_cache = *mut c_void;
pub type duckdb_database = *mut c_void;
pub type duckdb_connection = *mut c_void;
pub type duckdb_config = *mut c_void;
pub type duckdb_prepared_statement = *mut c_void;
pub type duckdb_extracted_statements = *mut c_void;
pub type duckdb_pending_result = *mut c_void;
pub type duckdb_logical_type = *mut c_void;
pub type duckdb_data_chunk = *mut c_void;
pub type duckdb_vector = *mut c_void;

/// `duckdb_result`: passed by pointer to most functions and by value to a few.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct duckdb_result {
    pub deprecated_column_count: idx_t,
    pub deprecated_row_count: idx_t,
    pub deprecated_rows_changed: idx_t,
    pub deprecated_columns: *mut c_void,
    pub deprecated_error_message: *mut c_char,
    pub internal_data: *mut c_void,
}

impl Default for duckdb_result {
    fn default() -> Self {
        Self {
            deprecated_column_count: 0,
            deprecated_row_count: 0,
            deprecated_rows_changed: 0,
            deprecated_columns: std::ptr::null_mut(),
            deprecated_error_message: std::ptr::null_mut(),
            internal_data: std::ptr::null_mut(),
        }
    }
}

/// `duckdb_string_t`: 16 bytes, inlined up to 12 bytes, otherwise a pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct duckdb_string_t {
    pub length: u32,
    pub data: [u8; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct duckdb_list_entry {
    pub offset: u64,
    pub length: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct duckdb_interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

/// `DUCKDB_TYPE` values used here.
pub mod types {
    pub const BOOLEAN: u32 = 1;
    pub const TINYINT: u32 = 2;
    pub const SMALLINT: u32 = 3;
    pub const INTEGER: u32 = 4;
    pub const BIGINT: u32 = 5;
    pub const UTINYINT: u32 = 6;
    pub const USMALLINT: u32 = 7;
    pub const UINTEGER: u32 = 8;
    pub const UBIGINT: u32 = 9;
    pub const FLOAT: u32 = 10;
    pub const DOUBLE: u32 = 11;
    pub const TIMESTAMP: u32 = 12;
    pub const DATE: u32 = 13;
    pub const TIME: u32 = 14;
    pub const INTERVAL: u32 = 15;
    pub const HUGEINT: u32 = 16;
    pub const VARCHAR: u32 = 17;
    pub const BLOB: u32 = 18;
    pub const DECIMAL: u32 = 19;
    pub const TIMESTAMP_S: u32 = 20;
    pub const TIMESTAMP_MS: u32 = 21;
    pub const TIMESTAMP_NS: u32 = 22;
    pub const ENUM: u32 = 23;
    pub const LIST: u32 = 24;
    pub const STRUCT: u32 = 25;
    pub const MAP: u32 = 26;
    pub const UUID: u32 = 27;
    pub const UNION: u32 = 28;
    pub const BIT: u32 = 29;
    pub const TIME_TZ: u32 = 30;
    pub const TIMESTAMP_TZ: u32 = 31;
    pub const UHUGEINT: u32 = 32;
    pub const ARRAY: u32 = 33;
    pub const SQLNULL: u32 = 36;
    pub const TIME_NS: u32 = 39;
    pub const TIMESTAMP_TZ_NS: u32 = 42;
}

/// `duckdb_statement_type` values used here.
pub const STATEMENT_SELECT: u32 = 1;
pub const STATEMENT_INSERT: u32 = 2;
pub const STATEMENT_UPDATE: u32 = 3;
pub const STATEMENT_DELETE: u32 = 5;

/// `duckdb_result_type`: a DML that reports its changed rows.
pub const RESULT_CHANGED_ROWS: u32 = 1;
/// `duckdb_result_type`: a command without rows (CREATE, SET…).
pub const RESULT_NOTHING: u32 = 2;

macro_rules! api {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// The functions of a loaded `libduckdb`. The library stays loaded while this lives.
        pub struct Api {
            _lib: libloading::Library,
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            /// Loads the library and every function the app uses; a library without one of them
            /// (another DuckDB version) is rejected here rather than failing later.
            pub fn load(path: &std::path::Path) -> anyhow::Result<Api> {
                // SAFETY: loading DuckDB's own library, whose initializers have no special requirements.
                let lib = unsafe { libloading::Library::new(path) }?;
                // SAFETY: each symbol is DuckDB's C function with the declared signature
                // (duckdb.h of the pinned version).
                unsafe {
                    $(let $name = *lib.get::<unsafe extern "C" fn($($arg),*) $(-> $ret)?>(concat!(stringify!($name), "\0").as_bytes())?;)*
                    Ok(Api { _lib: lib, $($name,)* })
                }
            }
        }
    };
}

api! {
    duckdb_free: fn(*mut c_void);
    duckdb_create_instance_cache: fn() -> duckdb_instance_cache;
    duckdb_get_or_create_from_cache: fn(duckdb_instance_cache, *const c_char, *mut duckdb_database, duckdb_config, *mut *mut c_char) -> duckdb_state;
    duckdb_create_config: fn(*mut duckdb_config) -> duckdb_state;
    duckdb_set_config: fn(duckdb_config, *const c_char, *const c_char) -> duckdb_state;
    duckdb_destroy_config: fn(*mut duckdb_config);
    duckdb_close: fn(*mut duckdb_database);
    duckdb_connect: fn(duckdb_database, *mut duckdb_connection) -> duckdb_state;
    duckdb_disconnect: fn(*mut duckdb_connection);
    duckdb_interrupt: fn(duckdb_connection);
    duckdb_extract_statements: fn(duckdb_connection, *const c_char, *mut duckdb_extracted_statements) -> idx_t;
    duckdb_prepare_extracted_statement: fn(duckdb_connection, duckdb_extracted_statements, idx_t, *mut duckdb_prepared_statement) -> duckdb_state;
    duckdb_extract_statements_error: fn(duckdb_extracted_statements) -> *const c_char;
    duckdb_destroy_extracted: fn(*mut duckdb_extracted_statements);
    duckdb_prepare_error: fn(duckdb_prepared_statement) -> *const c_char;
    duckdb_destroy_prepare: fn(*mut duckdb_prepared_statement);
    duckdb_nparams: fn(duckdb_prepared_statement) -> idx_t;
    duckdb_prepared_statement_type: fn(duckdb_prepared_statement) -> u32;
    duckdb_bind_varchar: fn(duckdb_prepared_statement, idx_t, *const c_char) -> duckdb_state;
    duckdb_pending_prepared_streaming: fn(duckdb_prepared_statement, *mut duckdb_pending_result) -> duckdb_state;
    duckdb_pending_error: fn(duckdb_pending_result) -> *const c_char;
    duckdb_execute_pending: fn(duckdb_pending_result, *mut duckdb_result) -> duckdb_state;
    duckdb_destroy_pending: fn(*mut duckdb_pending_result);
    duckdb_result_error: fn(*mut duckdb_result) -> *const c_char;
    duckdb_destroy_result: fn(*mut duckdb_result);
    duckdb_result_return_type: fn(duckdb_result) -> u32;
    duckdb_column_count: fn(*mut duckdb_result) -> idx_t;
    duckdb_column_name: fn(*mut duckdb_result, idx_t) -> *const c_char;
    duckdb_column_logical_type: fn(*mut duckdb_result, idx_t) -> duckdb_logical_type;
    duckdb_fetch_chunk: fn(duckdb_result) -> duckdb_data_chunk;
    duckdb_destroy_data_chunk: fn(*mut duckdb_data_chunk);
    duckdb_data_chunk_get_size: fn(duckdb_data_chunk) -> idx_t;
    duckdb_data_chunk_get_vector: fn(duckdb_data_chunk, idx_t) -> duckdb_vector;
    duckdb_vector_get_data: fn(duckdb_vector) -> *mut c_void;
    duckdb_vector_get_validity: fn(duckdb_vector) -> *mut u64;
    duckdb_list_vector_get_child: fn(duckdb_vector) -> duckdb_vector;
    duckdb_struct_vector_get_child: fn(duckdb_vector, idx_t) -> duckdb_vector;
    duckdb_array_vector_get_child: fn(duckdb_vector) -> duckdb_vector;
    duckdb_get_type_id: fn(duckdb_logical_type) -> u32;
    duckdb_destroy_logical_type: fn(*mut duckdb_logical_type);
    duckdb_logical_type_get_alias: fn(duckdb_logical_type) -> *mut c_char;
    duckdb_decimal_scale: fn(duckdb_logical_type) -> u8;
    duckdb_decimal_internal_type: fn(duckdb_logical_type) -> u32;
    duckdb_enum_internal_type: fn(duckdb_logical_type) -> u32;
    duckdb_enum_dictionary_size: fn(duckdb_logical_type) -> u32;
    duckdb_enum_dictionary_value: fn(duckdb_logical_type, idx_t) -> *mut c_char;
    duckdb_list_type_child_type: fn(duckdb_logical_type) -> duckdb_logical_type;
    duckdb_array_type_child_type: fn(duckdb_logical_type) -> duckdb_logical_type;
    duckdb_array_type_array_size: fn(duckdb_logical_type) -> idx_t;
    duckdb_struct_type_child_count: fn(duckdb_logical_type) -> idx_t;
    duckdb_struct_type_child_name: fn(duckdb_logical_type, idx_t) -> *mut c_char;
    duckdb_struct_type_child_type: fn(duckdb_logical_type, idx_t) -> duckdb_logical_type;
    duckdb_union_type_member_count: fn(duckdb_logical_type) -> idx_t;
    duckdb_union_type_member_name: fn(duckdb_logical_type, idx_t) -> *mut c_char;
    duckdb_union_type_member_type: fn(duckdb_logical_type, idx_t) -> duckdb_logical_type;
}

/// Text of a C string owned by DuckDB (not freed here); "" when null.
///
/// # Safety
/// `p` is null or a valid NUL-terminated string.
pub unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: guaranteed by the caller.
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

impl Api {
    /// Text of a C string DuckDB allocated for the caller, then freed with `duckdb_free`.
    ///
    /// # Safety
    /// `p` is null or a string DuckDB returned for the caller to free.
    pub unsafe fn owned_text(&self, p: *mut c_char) -> String {
        // SAFETY: guaranteed by the caller.
        let s = unsafe { text(p) };
        if !p.is_null() {
            // SAFETY: DuckDB allocated it and hands ownership over.
            unsafe { (self.duckdb_free)(p.cast()) };
        }
        s
    }
}
