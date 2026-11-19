//! DuckDB result vectors to the grid's text cells, type by type (the C API hands out raw
//! columnar data, not strings).

use std::fmt::Write as _;

use super::ffi::{self, Api, duckdb_interval, duckdb_list_entry, duckdb_logical_type, duckdb_string_t, duckdb_vector, types as t};
use crate::db::Cell;

/// A column's type, resolved once per result rather than per cell.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    I128,
    U128,
    F32,
    F64,
    /// Fixed point stored as an integer of `internal` type.
    Decimal { scale: u8, internal: u32 },
    Varchar,
    Blob,
    Bit,
    Date,
    /// Microseconds since midnight.
    Time,
    TimeNs,
    TimeTz,
    /// Since the epoch, in the unit's fraction of a second (1 = seconds … 1e9 = nanoseconds).
    Timestamp { per_second: i64, tz: bool },
    Interval,
    Uuid,
    Enum { internal: u32, values: Vec<String> },
    List(Box<Ty>),
    Array(Box<Ty>, u64),
    Struct(Vec<(String, Ty)>),
    Map(Box<Ty>, Box<Ty>),
    Union(Vec<(String, Ty)>),
    Null,
    /// A type this version doesn't render (shown by name).
    Other(String),
}

impl Ty {
    /// Resolves a logical type (not destroyed here).
    ///
    /// # Safety
    /// `ty` is a valid logical type.
    pub unsafe fn resolve(api: &Api, ty: duckdb_logical_type) -> Ty {
        // SAFETY: every call receives the valid `ty` or a child type DuckDB just returned, each
        // destroyed after use.
        unsafe {
            let child = |c: duckdb_logical_type| {
                let resolved = Ty::resolve(api, c);
                let mut c = c;
                (api.duckdb_destroy_logical_type)(&mut c);
                resolved
            };
            match (api.duckdb_get_type_id)(ty) {
                t::BOOLEAN => Ty::Bool,
                t::TINYINT => Ty::I8,
                t::SMALLINT => Ty::I16,
                t::INTEGER => Ty::I32,
                t::BIGINT => Ty::I64,
                t::UTINYINT => Ty::U8,
                t::USMALLINT => Ty::U16,
                t::UINTEGER => Ty::U32,
                t::UBIGINT => Ty::U64,
                t::HUGEINT => Ty::I128,
                t::UHUGEINT => Ty::U128,
                t::FLOAT => Ty::F32,
                t::DOUBLE => Ty::F64,
                t::DECIMAL => Ty::Decimal { scale: (api.duckdb_decimal_scale)(ty), internal: (api.duckdb_decimal_internal_type)(ty) },
                t::VARCHAR => Ty::Varchar,
                t::BLOB => Ty::Blob,
                t::BIT => Ty::Bit,
                t::DATE => Ty::Date,
                t::TIME => Ty::Time,
                t::TIME_NS => Ty::TimeNs,
                t::TIME_TZ => Ty::TimeTz,
                t::TIMESTAMP => Ty::Timestamp { per_second: 1_000_000, tz: false },
                t::TIMESTAMP_S => Ty::Timestamp { per_second: 1, tz: false },
                t::TIMESTAMP_MS => Ty::Timestamp { per_second: 1_000, tz: false },
                t::TIMESTAMP_NS => Ty::Timestamp { per_second: 1_000_000_000, tz: false },
                t::TIMESTAMP_TZ => Ty::Timestamp { per_second: 1_000_000, tz: true },
                t::TIMESTAMP_TZ_NS => Ty::Timestamp { per_second: 1_000_000_000, tz: true },
                t::INTERVAL => Ty::Interval,
                t::UUID => Ty::Uuid,
                t::ENUM => {
                    let n = (api.duckdb_enum_dictionary_size)(ty);
                    let values = (0..n).map(|i| api.owned_text((api.duckdb_enum_dictionary_value)(ty, i as u64))).collect();
                    Ty::Enum { internal: (api.duckdb_enum_internal_type)(ty), values }
                }
                t::LIST => Ty::List(Box::new(child((api.duckdb_list_type_child_type)(ty)))),
                t::ARRAY => Ty::Array(Box::new(child((api.duckdb_array_type_child_type)(ty))), (api.duckdb_array_type_array_size)(ty)),
                t::STRUCT => Ty::Struct(
                    (0..(api.duckdb_struct_type_child_count)(ty))
                        .map(|i| (api.owned_text((api.duckdb_struct_type_child_name)(ty, i)), child((api.duckdb_struct_type_child_type)(ty, i))))
                        .collect(),
                ),
                // A list of `STRUCT(key, value)`.
                t::MAP => match child((api.duckdb_list_type_child_type)(ty)) {
                    Ty::Struct(mut kv) if kv.len() == 2 => {
                        let value = kv.pop().map(|(_, v)| v).unwrap_or(Ty::Null);
                        let key = kv.pop().map(|(_, k)| k).unwrap_or(Ty::Null);
                        Ty::Map(Box::new(key), Box::new(value))
                    }
                    _ => Ty::Other("MAP".into()),
                },
                t::UNION => Ty::Union(
                    (0..(api.duckdb_union_type_member_count)(ty))
                        .map(|i| (api.owned_text((api.duckdb_union_type_member_name)(ty, i)), child((api.duckdb_union_type_member_type)(ty, i))))
                        .collect(),
                ),
                t::SQLNULL => Ty::Null,
                id => {
                    let alias = api.owned_text((api.duckdb_logical_type_get_alias)(ty));
                    Ty::Other(if alias.is_empty() { format!("type {id}") } else { alias })
                }
            }
        }
    }
}

/// Reads the `row`-th element of a vector's data as `T`.
///
/// # Safety
/// `vector` holds at least `row + 1` elements of type `T`.
unsafe fn read<T: Copy>(api: &Api, vector: duckdb_vector, row: u64) -> T {
    // SAFETY: guaranteed by the caller; vector data is aligned for its element type.
    unsafe { *((api.duckdb_vector_get_data)(vector) as *const T).add(row as usize) }
}

/// The bytes of a `duckdb_string_t` (VARCHAR, BLOB, BIT).
///
/// # Safety
/// `s` comes from a live vector.
unsafe fn string_bytes(s: &duckdb_string_t) -> &[u8] {
    let len = s.length as usize;
    if len <= 12 {
        &s.data[..len]
    } else {
        // Bytes 4..12 are the pointer to the full string (after a 4-byte prefix).
        // SAFETY: the layout of a non-inlined `duckdb_string_t`, valid while the chunk lives.
        unsafe {
            let ptr = std::ptr::read_unaligned(s.data[4..12].as_ptr() as *const *const u8);
            std::slice::from_raw_parts(ptr, len)
        }
    }
}

/// Whether the `row`-th value of a vector is not NULL.
///
/// # Safety
/// `vector` is valid and holds `row`.
pub unsafe fn is_valid(api: &Api, vector: duckdb_vector, row: u64) -> bool {
    // SAFETY: the validity mask (when there is one) covers every row of the vector.
    unsafe {
        let mask = (api.duckdb_vector_get_validity)(vector);
        mask.is_null() || (*mask.add((row / 64) as usize) >> (row % 64)) & 1 == 1
    }
}

/// The `row`-th value of a vector as a grid cell (None = NULL).
///
/// # Safety
/// `vector` is valid, of type `ty`, and holds `row`.
pub unsafe fn cell(api: &Api, vector: duckdb_vector, ty: &Ty, row: u64) -> Cell {
    // SAFETY: guaranteed by the caller.
    unsafe {
        if !is_valid(api, vector, row) {
            return None;
        }
        Some(render(api, vector, ty, row, false))
    }
}

/// The text of a valid value; `nested` quotes strings, as inside a list or struct.
///
/// # Safety
/// As `cell`, and the value is not NULL.
unsafe fn render(api: &Api, v: duckdb_vector, ty: &Ty, row: u64, nested: bool) -> String {
    // SAFETY: each read uses the element type of `ty`, the vector's own type.
    unsafe {
        match ty {
            Ty::Bool => read::<bool>(api, v, row).to_string(),
            Ty::I8 => read::<i8>(api, v, row).to_string(),
            Ty::I16 => read::<i16>(api, v, row).to_string(),
            Ty::I32 => read::<i32>(api, v, row).to_string(),
            Ty::I64 => read::<i64>(api, v, row).to_string(),
            Ty::U8 => read::<u8>(api, v, row).to_string(),
            Ty::U16 => read::<u16>(api, v, row).to_string(),
            Ty::U32 => read::<u32>(api, v, row).to_string(),
            Ty::U64 => read::<u64>(api, v, row).to_string(),
            Ty::I128 => read_i128(api, v, row).to_string(),
            Ty::U128 => {
                let (lower, upper) = read::<(u64, u64)>(api, v, row);
                (((upper as u128) << 64) | lower as u128).to_string()
            }
            Ty::F32 => float(read::<f32>(api, v, row) as f64, true),
            Ty::F64 => float(read::<f64>(api, v, row), false),
            Ty::Decimal { scale, internal } => {
                let n: i128 = match *internal {
                    t::SMALLINT => read::<i16>(api, v, row) as i128,
                    t::INTEGER => read::<i32>(api, v, row) as i128,
                    t::BIGINT => read::<i64>(api, v, row) as i128,
                    _ => read_i128(api, v, row),
                };
                decimal(n, *scale)
            }
            Ty::Varchar => {
                let s = String::from_utf8_lossy(string_bytes(&read::<duckdb_string_t>(api, v, row))).into_owned();
                if nested { format!("'{}'", s.replace('\'', "''")) } else { s }
            }
            Ty::Blob => crate::db::sqlite::hex_preview(string_bytes(&read::<duckdb_string_t>(api, v, row))),
            Ty::Bit => bits(string_bytes(&read::<duckdb_string_t>(api, v, row))),
            Ty::Date => date(read::<i32>(api, v, row)),
            Ty::Time => time_of_day(read::<i64>(api, v, row) as i128 * 1_000),
            Ty::TimeNs => time_of_day(read::<i64>(api, v, row) as i128),
            Ty::TimeTz => time_tz(read::<u64>(api, v, row)),
            Ty::Timestamp { per_second, tz } => timestamp(read::<i64>(api, v, row), *per_second, *tz),
            Ty::Interval => interval(read::<duckdb_interval>(api, v, row)),
            Ty::Uuid => uuid(read_i128(api, v, row)),
            Ty::Enum { internal, values } => {
                let i = match *internal {
                    t::UTINYINT => read::<u8>(api, v, row) as usize,
                    t::USMALLINT => read::<u16>(api, v, row) as usize,
                    _ => read::<u32>(api, v, row) as usize,
                };
                let s = values.get(i).cloned().unwrap_or_default();
                if nested { format!("'{}'", s.replace('\'', "''")) } else { s }
            }
            Ty::List(child) => {
                let entry = read::<duckdb_list_entry>(api, v, row);
                let items = (api.duckdb_list_vector_get_child)(v);
                let parts: Vec<String> = (entry.offset..entry.offset + entry.length).map(|i| element(api, items, child, i)).collect();
                format!("[{}]", parts.join(", "))
            }
            Ty::Array(child, size) => {
                let items = (api.duckdb_array_vector_get_child)(v);
                let parts: Vec<String> = (row * size..(row + 1) * size).map(|i| element(api, items, child, i)).collect();
                format!("[{}]", parts.join(", "))
            }
            Ty::Struct(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .enumerate()
                    .map(|(i, (name, fty))| format!("'{}': {}", name.replace('\'', "''"), element(api, (api.duckdb_struct_vector_get_child)(v, i as u64), fty, row)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Ty::Map(kty, vty) => {
                let entry = read::<duckdb_list_entry>(api, v, row);
                let pairs = (api.duckdb_list_vector_get_child)(v);
                let (keys, values) = ((api.duckdb_struct_vector_get_child)(pairs, 0), (api.duckdb_struct_vector_get_child)(pairs, 1));
                let parts: Vec<String> = (entry.offset..entry.offset + entry.length)
                    .map(|i| format!("{}={}", element(api, keys, kty, i), element(api, values, vty, i)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            // Child 0 is the tag; member `i` is child `i + 1`.
            Ty::Union(members) => {
                let tag = read::<u8>(api, (api.duckdb_struct_vector_get_child)(v, 0), row) as usize;
                match members.get(tag) {
                    Some((_, mty)) => element(api, (api.duckdb_struct_vector_get_child)(v, tag as u64 + 1), mty, row),
                    None => String::new(),
                }
            }
            Ty::Null => "NULL".into(),
            Ty::Other(name) => format!("<{name}>"),
        }
    }
}

/// A nested value: NULL spelled out, strings quoted.
///
/// # Safety
/// As `render`.
unsafe fn element(api: &Api, v: duckdb_vector, ty: &Ty, row: u64) -> String {
    // SAFETY: guaranteed by the caller.
    unsafe { if is_valid(api, v, row) { render(api, v, ty, row, true) } else { "NULL".into() } }
}

/// HUGEINT / UUID storage: lower then upper half.
///
/// # Safety
/// The vector holds 16-byte elements.
unsafe fn read_i128(api: &Api, v: duckdb_vector, row: u64) -> i128 {
    // SAFETY: guaranteed by the caller.
    let (lower, upper) = unsafe { read::<(u64, i64)>(api, v, row) };
    ((upper as i128) << 64) | lower as i128
}

fn float(f: f64, single: bool) -> String {
    if f.is_nan() {
        "nan".into()
    } else if f.is_infinite() {
        if f > 0.0 { "inf".into() } else { "-inf".into() }
    } else if single {
        // The shortest text that reads back as the same f32.
        (f as f32).to_string()
    } else {
        f.to_string()
    }
}

pub(super) fn decimal(n: i128, scale: u8) -> String {
    if scale == 0 {
        return n.to_string();
    }
    let digits = n.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits };
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int}.{frac}", if n < 0 { "-" } else { "" })
}

/// BIT: the first byte is the number of padding bits at the start of the second.
fn bits(bytes: &[u8]) -> String {
    let Some((&padding, data)) = bytes.split_first() else { return String::new() };
    let mut s = String::with_capacity(data.len() * 8);
    for (i, b) in data.iter().enumerate() {
        let skip = if i == 0 { padding as usize } else { 0 };
        for bit in skip..8 {
            s.push(if b & (0x80 >> bit) != 0 { '1' } else { '0' });
        }
    }
    s
}

const INFINITY_DATE: i32 = i32::MAX;
const INFINITY_TIMESTAMP: i64 = i64::MAX;

pub(super) fn date(days: i32) -> String {
    match days {
        INFINITY_DATE => "infinity".into(),
        d if d == -INFINITY_DATE => "-infinity".into(),
        d => match chrono::NaiveDate::from_num_days_from_ce_opt(d.saturating_add(719_163)) {
            Some(date) => date_text(date),
            None => d.to_string(),
        },
    }
}

/// DuckDB's spelling: years before 1 as `(BC)`.
fn date_text(date: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    if date.year() > 0 {
        date.format("%Y-%m-%d").to_string()
    } else {
        format!("{:04}-{:02}-{:02} (BC)", 1 - date.year(), date.month(), date.day())
    }
}

/// `HH:MM:SS` plus the fraction without trailing zeros, from nanoseconds since midnight.
pub(super) fn time_of_day(nanos: i128) -> String {
    let secs = nanos.div_euclid(1_000_000_000);
    let frac = nanos.rem_euclid(1_000_000_000) as u32;
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    fraction(&mut s, frac);
    s
}

fn fraction(s: &mut String, nanos: u32) {
    if nanos != 0 {
        let digits = format!("{nanos:09}");
        // Microseconds unless there are nanoseconds.
        let shown = if digits.ends_with("000") { &digits[..6] } else { &digits[..] };
        let _ = write!(s, ".{}", shown.trim_end_matches('0'));
    }
}

/// TIME WITH TIME ZONE: micros in the upper 40 bits, the offset (encoded from its maximum) in
/// the lower 24.
fn time_tz(bits: u64) -> String {
    const MAX_OFFSET: i64 = 16 * 60 * 60 - 1;
    let micros = (bits >> 24) as i128;
    let offset = MAX_OFFSET - (bits & 0xFF_FFFF) as i64;
    let mut s = time_of_day(micros * 1_000);
    s.push_str(&utc_offset(offset));
    s
}

fn utc_offset(seconds: i64) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let a = seconds.abs();
    let mut s = format!("{sign}{:02}", a / 3600);
    if a % 3600 != 0 {
        let _ = write!(s, ":{:02}", a / 60 % 60);
    }
    if a % 60 != 0 {
        let _ = write!(s, ":{:02}", a % 60);
    }
    s
}

/// A TIMESTAMP (or WITH TIME ZONE, shown in UTC as `+00`).
pub(super) fn timestamp(value: i64, per_second: i64, tz: bool) -> String {
    if value == INFINITY_TIMESTAMP {
        return "infinity".into();
    }
    if value == -INFINITY_TIMESTAMP {
        return "-infinity".into();
    }
    let secs = value.div_euclid(per_second);
    let nanos = (value.rem_euclid(per_second) as i128 * 1_000_000_000 / per_second as i128) as u32;
    let Some(dt) = chrono::DateTime::from_timestamp(secs, 0) else { return value.to_string() };
    let dt = dt.naive_utc();
    let mut s = format!("{} {}", date_text(dt.date()), dt.format("%H:%M:%S"));
    fraction(&mut s, nanos);
    if tz {
        s.push_str("+00");
    }
    s
}

/// DuckDB's interval text: `1 year 2 months 3 days 04:05:06.7`.
pub(super) fn interval(i: duckdb_interval) -> String {
    let mut parts = Vec::new();
    let (years, months) = (i.months / 12, i.months % 12);
    let unit = |n: i32, one: &str| format!("{n} {one}{}", if n.abs() == 1 { "" } else { "s" });
    if years != 0 {
        parts.push(unit(years, "year"));
    }
    if months != 0 {
        parts.push(if months.abs() == 1 { format!("{months} month") } else { format!("{months} months") });
    }
    if i.days != 0 {
        parts.push(unit(i.days, "day"));
    }
    if i.micros != 0 || parts.is_empty() {
        let neg = i.micros < 0;
        let t = time_of_day(i.micros.unsigned_abs() as i128 * 1_000);
        parts.push(if neg { format!("-{t}") } else { t });
    }
    parts.join(" ")
}

/// UUID storage flips the top bit so they sort as text.
pub(super) fn uuid(stored: i128) -> String {
    let n = (stored as u128) ^ (1u128 << 127);
    let h = format!("{n:032x}");
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Every cell of the first `rows` rows of a chunk's column.
///
/// # Safety
/// `vector` is the column of a live chunk of at least `rows` rows, of type `ty`.
pub unsafe fn column(api: &Api, vector: ffi::duckdb_vector, ty: &Ty, rows: u64) -> Vec<Cell> {
    // SAFETY: guaranteed by the caller.
    (0..rows).map(|r| unsafe { cell(api, vector, ty, r) }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_formats_match_duckdb() {
        assert_eq!(decimal(12345, 2), "123.45");
        assert_eq!(decimal(-5, 3), "-0.005");
        assert_eq!(decimal(7, 0), "7");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(-1), "1969-12-31");
        assert_eq!(date(i32::MAX), "infinity");
        assert_eq!(time_of_day(3_723_500_000_000), "01:02:03.5");
        assert_eq!(time_of_day(0), "00:00:00");
        assert_eq!(timestamp(1_700_000_000_123_456, 1_000_000, false), "2023-11-14 22:13:20.123456");
        assert_eq!(timestamp(1_700_000_000, 1, true), "2023-11-14 22:13:20+00");
        assert_eq!(timestamp(-1, 1_000_000, false), "1969-12-31 23:59:59.999999");
        assert_eq!(timestamp(1_000_000_001, 1_000_000_000, false), "1970-01-01 00:00:01.000000001");
        assert_eq!(interval(duckdb_interval { months: 14, days: 3, micros: 4 * 3_600_000_000 + 5 * 60_000_000 + 6_700_000 }), "1 year 2 months 3 days 04:05:06.7");
        assert_eq!(interval(duckdb_interval { months: 0, days: 0, micros: 0 }), "00:00:00");
        assert_eq!(bits(&[3, 0b0001_0110]), "10110");
        assert_eq!(utc_offset(-(5 * 3600 + 30 * 60)), "-05:30");
        assert_eq!(float(0.1, false), "0.1");
        assert_eq!(float(0.1f32 as f64, true), "0.1");
    }
}
