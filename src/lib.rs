//! Rusqlite is an ergonomic wrapper for using SQLite from Rust. It attempts to
//! expose an interface similar to [rust-postgres](https://github.com/sfackler/rust-postgres).
//!
//! ```rust
//! use rusqlite::types::ToSql;
//! use rusqlite::{params, Connection, Result};
//! use time::Timespec;
//!
//! #[derive(Debug)]
//! struct Person {
//!     id: i32,
//!     name: String,
//!     time_created: Timespec,
//!     data: Option<Vec<u8>>,
//! }
//!
//! fn main() -> Result<()> {
//!     let conn = Connection::open_in_memory()?;
//!
//!     conn.execute(
//!         "CREATE TABLE person (
//!                   id              INTEGER PRIMARY KEY,
//!                   name            TEXT NOT NULL,
//!                   time_created    TEXT NOT NULL,
//!                   data            BLOB
//!                   )",
//!         params![],
//!     )?;
//!     let me = Person {
//!         id: 0,
//!         name: "Steven".to_string(),
//!         time_created: time::get_time(),
//!         data: None,
//!     };
//!     conn.execute(
//!         "INSERT INTO person (name, time_created, data)
//!                   VALUES (?1, ?2, ?3)",
//!         params![me.name, me.time_created, me.data],
//!     )?;
//!
//!     let mut stmt = conn.prepare("SELECT id, name, time_created, data FROM person")?;
//!     let person_iter = stmt.query_map(params![], |row| {
//!         Ok(Person {
//!             id: row.get(0)?,
//!             name: row.get(1)?,
//!             time_created: row.get(2)?,
//!             data: row.get(3)?,
//!         })
//!     })?;
//!
//!     for person in person_iter {
//!         println!("Found person {:?}", person.unwrap());
//!     }
//!     Ok(())
//! }
//! ```
#![allow(unknown_lints)]

pub use libsqlite3_sys as ffi;

#[macro_use]
extern crate bitflags;
#[cfg(any(test, feature = "vtab"))]
#[macro_use]
extern crate lazy_static;

use std::cell::RefCell;
use std::convert;
use std::default::Default;
use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::{c_char, c_int};

use std::path::{Path, PathBuf};
use std::result;
use std::str;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::cache::StatementCache;
use crate::inner_connection::{InnerConnection, BYPASS_SQLITE_INIT};
use crate::raw_statement::RawStatement;
use crate::types::ValueRef;

pub use crate::cache::CachedStatement;
pub use crate::column::Column;
pub use crate::error::Error;
pub use crate::ffi::ErrorCode;
#[cfg(feature = "hooks")]
pub use crate::hooks::Action;
#[cfg(feature = "load_extension")]
pub use crate::load_extension_guard::LoadExtensionGuard;
pub use crate::row::{AndThenRows, MappedRows, Row, RowIndex, Rows};
pub use crate::statement::{Statement, StatementStatus};
pub use crate::transaction::{DropBehavior, Savepoint, Transaction, TransactionBehavior};
pub use crate::types::ToSql;
pub use crate::version::*;

#[macro_use]
mod error;

#[cfg(feature = "backup")]
pub mod backup;
#[cfg(feature = "blob")]
pub mod blob;
mod busy;
mod cache;
#[cfg(feature = "collation")]
mod collation;
mod column;
pub mod config;
#[cfg(any(feature = "functions", feature = "vtab"))]
mod context;
#[cfg(feature = "functions")]
pub mod functions;
#[cfg(feature = "hooks")]
mod hooks;
mod inner_connection;
#[cfg(feature = "limits")]
pub mod limits;
#[cfg(feature = "load_extension")]
mod load_extension_guard;
mod pragma;
mod raw_statement;
mod row;
#[cfg(feature = "session")]
pub mod session;
mod statement;
#[cfg(feature = "trace")]
pub mod trace;
mod transaction;
pub mod types;
mod unlock_notify;
mod version;
#[cfg(feature = "vtab")]
pub mod vtab;

// Number of cached prepared statements we'll hold on to.
const STATEMENT_CACHE_DEFAULT_CAPACITY: usize = 16;
/// To be used when your statement has no [parameter](https://sqlite.org/lang_expr.html#varparam).
pub const NO_PARAMS: &[&dyn ToSql] = &[];

/// A macro making it more convenient to pass heterogeneous lists
/// of parameters as a `&[&dyn ToSql]`.
///
/// # Example
///
/// ```rust,no_run
/// # use rusqlite::{Result, Connection, params};
///
/// struct Person {
///     name: String,
///     age_in_years: u8,
///     data: Option<Vec<u8>>,
/// }
///
/// fn add_person(conn: &Connection, person: &Person) -> Result<()> {
///     conn.execute("INSERT INTO person (name, age_in_years, data)
///                   VALUES (?1, ?2, ?3)",
///                  params![person.name, person.age_in_years, person.data])?;
///     Ok(())
/// }
/// ```
#[macro_export]
macro_rules! params {
    () => {
        $crate::NO_PARAMS
    };
    ($($param:expr),+ $(,)?) => {
        &[$(&$param as &dyn $crate::ToSql),+]
    };
}

/// A macro making it more convenient to pass lists of named parameters
/// as a `&[(&str, &dyn ToSql)]`.
///
/// # Example
///
/// ```rust,no_run
/// # use rusqlite::{Result, Connection, named_params};
///
/// struct Person {
///     name: String,
///     age_in_years: u8,
///     data: Option<Vec<u8>>,
/// }
///
/// fn add_person(conn: &Connection, person: &Person) -> Result<()> {
///     conn.execute_named(
///         "INSERT INTO person (name, age_in_years, data)
///          VALUES (:name, :age, :data)",
///         named_params!{
///             ":name": person.name,
///             ":age": person.age_in_years,
///             ":data": person.data,
///         }
///     )?;
///     Ok(())
/// }
/// ```
#[macro_export]
macro_rules! named_params {
    () => {
        &[]
    };
    // Note: It's a lot more work to support this as part of the same macro as
    // `params!`, unfortunately.
    ($($param_name:literal: $param_val:expr),+ $(,)?) => {
        &[$(($param_name, &$param_val as &dyn $crate::ToSql)),+]
    };
}

/// A typedef of the result returned by many methods.
pub type Result<T> = result::Result<T, Error>;

/// See the [method documentation](#tymethod.optional).
pub trait OptionalExtension<T> {
    /// Converts a `Result<T>` into a `Result<Option<T>>`.
    ///
    /// By default, Rusqlite treats 0 rows being returned from a query that is
    /// expected to return 1 row as an error. This method will
    /// handle that error, and give you back an `Option<T>` instead.
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

unsafe fn errmsg_to_string(errmsg: *const c_char) -> String {
    let c_slice = CStr::from_ptr(errmsg).to_bytes();
    String::from_utf8_lossy(c_slice).into_owned()
}

fn str_to_cstring(s: &str) -> Result<CString> {
    Ok(CString::new(s)?)
}

/// Returns `Ok((string ptr, len as c_int, SQLITE_STATIC | SQLITE_TRANSIENT))`
/// normally.
/// Returns errors if the string has embedded nuls or is too large for sqlite.
/// The `sqlite3_destructor_type` item is always `SQLITE_TRANSIENT` unless
/// the string was empty (in which case it's `SQLITE_STATIC`, and the ptr is
/// static).
fn str_for_sqlite(s: &str) -> Result<(*const c_char, c_int, ffi::sqlite3_destructor_type)> {
    let len = len_as_c_int(s.len())?;
    if memchr::memchr(0, s.as_bytes()).is_none() {
        let (ptr, dtor_info) = if len != 0 {
            (s.as_ptr() as *const c_char, ffi::SQLITE_TRANSIENT())
        } else {
            // Return a pointer guaranteed to live forever
            ("".as_ptr() as *const c_char, ffi::SQLITE_STATIC())
        };
        Ok((ptr, len, dtor_info))
    } else {
        // There's an embedded nul, so we fabricate a NulError.
        let e = CString::new(s);
        Err(Error::NulError(e.unwrap_err()))
    }
}

// Helper to cast to c_int safely, returning the correct error type if the cast
// failed.
fn len_as_c_int(len: usize) -> Result<c_int> {
    if len >= (c_int::max_value() as usize) {
        Err(Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_TOOBIG),
            None,
        ))
    } else {
        Ok(len as c_int)
    }
}

fn path_to_cstring(p: &Path) -> Result<CString> {
    let s = p.to_str().ok_or_else(|| Error::InvalidPath(p.to_owned()))?;
    str_to_cstring(s)
}

/// Name for a database within a SQLite connection.
#[derive(Copy, Clone)]
pub enum DatabaseName<'a> {
    /// The main database.
    Main,

    /// The temporary database (e.g., any "CREATE TEMPORARY TABLE" tables).
    Temp,

    /// A database that has been attached via "ATTACH DATABASE ...".
    Attached(&'a str),
}

// Currently DatabaseName is only used by the backup and blob mods, so hide
// this (private) impl to avoid dead code warnings.
#[cfg(any(
    feature = "backup",
    feature = "blob",
    feature = "session",
    feature = "bundled"
))]
impl DatabaseName<'_> {
    fn to_cstring(&self) -> Result<CString> {
        use self::DatabaseName::{Attached, Main, Temp};
        match *self {
            Main => str_to_cstring("main"),
            Temp => str_to_cstring("temp"),
            Attached(s) => str_to_cstring(s),
        }
    }
}

/// A connection to a SQLite database.
pub struct Connection {
    db: RefCell<InnerConnection>,
    cache: StatementCache,
    path: Option<PathBuf>,
}

unsafe impl Send for Connection {}

impl Drop for Connection {
    fn drop(&mut self) {
        self.flush_prepared_statement_cache();
    }
}

impl Connection {
    /// Open a new connection to a SQLite database.
    ///
    /// `Connection::open(path)` is equivalent to
    /// `Connection::open_with_flags(path,
    /// OpenFlags::SQLITE_OPEN_READ_WRITE |
    /// OpenFlags::SQLITE_OPEN_CREATE)`.
    ///
    /// # Failure
    ///
    /// Will return `Err` if `path` cannot be converted to a C-compatible
    /// string or if the underlying SQLite open call fails.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Connection> {
        let flags = OpenFlags::default();
        Connection::open_with_flags(path, flags)
    }

    /// Open a new connection to an in-memory SQLite database.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite open call fails.
    pub fn open_in_memory() -> Result<Connection> {
        let flags = OpenFlags::default();
        Connection::open_in_memory_with_flags(flags)
    }

    /// Open a new connection to a SQLite database.
    ///
    /// [Database Connection](http://www.sqlite.org/c3ref/open.html) for a description of valid
    /// flag combinations.
    ///
    /// # Failure
    ///
    /// Will return `Err` if `path` cannot be converted to a C-compatible
    /// string or if the underlying SQLite open call fails.
    pub fn open_with_flags<P: AsRef<Path>>(path: P, flags: OpenFlags) -> Result<Connection> {
        let c_path = path_to_cstring(path.as_ref())?;
        InnerConnection::open_with_flags(&c_path, flags).map(|db| Connection {
            db: RefCell::new(db),
            cache: StatementCache::with_capacity(STATEMENT_CACHE_DEFAULT_CAPACITY),
            path: Some(path.as_ref().to_path_buf()),
        })
    }

    /// Open a new connection to an in-memory SQLite database.
    ///
    /// [Database Connection](http://www.sqlite.org/c3ref/open.html) for a description of valid
    /// flag combinations.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite open call fails.
    pub fn open_in_memory_with_flags(flags: OpenFlags) -> Result<Connection> {
        let c_memory = str_to_cstring(":memory:")?;
        InnerConnection::open_with_flags(&c_memory, flags).map(|db| Connection {
            db: RefCell::new(db),
            cache: StatementCache::with_capacity(STATEMENT_CACHE_DEFAULT_CAPACITY),
            path: None,
        })
    }

    /// Convenience method to run multiple SQL statements (that cannot take any
    /// parameters).
    ///
    /// Uses [sqlite3_exec](http://www.sqlite.org/c3ref/exec.html) under the hood.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// fn create_tables(conn: &Connection) -> Result<()> {
    ///     conn.execute_batch(
    ///         "BEGIN;
    ///                         CREATE TABLE foo(x INTEGER);
    ///                         CREATE TABLE bar(y TEXT);
    ///                         COMMIT;",
    ///     )
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if `sql` cannot be converted to a C-compatible string
    /// or if the underlying SQLite call fails.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.db.borrow_mut().execute_batch(sql)
    }

    /// Convenience method to prepare and execute a single SQL statement.
    ///
    /// On success, returns the number of rows that were changed or inserted or
    /// deleted (via `sqlite3_changes`).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection};
    /// fn update_rows(conn: &Connection) {
    ///     match conn.execute("UPDATE foo SET bar = 'baz' WHERE qux = ?", &[1i32]) {
    ///         Ok(updated) => println!("{} rows were updated", updated),
    ///         Err(err) => println!("update failed: {}", err),
    ///     }
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if `sql` cannot be converted to a C-compatible string
    /// or if the underlying SQLite call fails.
    pub fn execute<P>(&self, sql: &str, params: P) -> Result<usize>
    where
        P: IntoIterator,
        P::Item: ToSql,
    {
        self.prepare(sql).and_then(|mut stmt| stmt.execute(params))
    }

    /// Convenience method to prepare and execute a single SQL statement with
    /// named parameter(s).
    ///
    /// On success, returns the number of rows that were changed or inserted or
    /// deleted (via `sqlite3_changes`).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// fn insert(conn: &Connection) -> Result<usize> {
    ///     conn.execute_named(
    ///         "INSERT INTO test (name) VALUES (:name)",
    ///         &[(":name", &"one")],
    ///     )
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if `sql` cannot be converted to a C-compatible string
    /// or if the underlying SQLite call fails.
    pub fn execute_named(&self, sql: &str, params: &[(&str, &dyn ToSql)]) -> Result<usize> {
        self.prepare(sql)
            .and_then(|mut stmt| stmt.execute_named(params))
    }

    /// Get the SQLite rowid of the most recent successful INSERT.
    ///
    /// Uses [sqlite3_last_insert_rowid](https://www.sqlite.org/c3ref/last_insert_rowid.html) under
    /// the hood.
    pub fn last_insert_rowid(&self) -> i64 {
        self.db.borrow_mut().last_insert_rowid()
    }

    /// Convenience method to execute a query that is expected to return a
    /// single row.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Result,Connection, NO_PARAMS};
    /// fn preferred_locale(conn: &Connection) -> Result<String> {
    ///     conn.query_row(
    ///         "SELECT value FROM preferences WHERE name='locale'",
    ///         NO_PARAMS,
    ///         |row| row.get(0),
    ///     )
    /// }
    /// ```
    ///
    /// If the query returns more than one row, all rows except the first are
    /// ignored.
    ///
    /// Returns `Err(QueryReturnedNoRows)` if no results are returned. If the
    /// query truly is optional, you can call `.optional()` on the result of
    /// this to get a `Result<Option<T>>`.
    ///
    /// # Failure
    ///
    /// Will return `Err` if `sql` cannot be converted to a C-compatible string
    /// or if the underlying SQLite call fails.
    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        P: IntoIterator,
        P::Item: ToSql,
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let mut stmt = self.prepare(sql)?;
        stmt.query_row(params, f)
    }

    /// Convenience method to execute a query with named parameter(s) that is
    /// expected to return a single row.
    ///
    /// If the query returns more than one row, all rows except the first are
    /// ignored.
    ///
    /// Returns `Err(QueryReturnedNoRows)` if no results are returned. If the
    /// query truly is optional, you can call `.optional()` on the result of
    /// this to get a `Result<Option<T>>`.
    ///
    /// # Failure
    ///
    /// Will return `Err` if `sql` cannot be converted to a C-compatible string
    /// or if the underlying SQLite call fails.
    pub fn query_row_named<T, F>(&self, sql: &str, params: &[(&str, &dyn ToSql)], f: F) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let mut stmt = self.prepare(sql)?;
        stmt.query_row_named(params, f)
    }

    /// Convenience method to execute a query that is expected to return a
    /// single row, and execute a mapping via `f` on that returned row with
    /// the possibility of failure. The `Result` type of `f` must implement
    /// `std::convert::From<Error>`.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Result,Connection, NO_PARAMS};
    /// fn preferred_locale(conn: &Connection) -> Result<String> {
    ///     conn.query_row_and_then(
    ///         "SELECT value FROM preferences WHERE name='locale'",
    ///         NO_PARAMS,
    ///         |row| row.get(0),
    ///     )
    /// }
    /// ```
    ///
    /// If the query returns more than one row, all rows except the first are
    /// ignored.
    ///
    /// # Failure
    ///
    /// Will return `Err` if `sql` cannot be converted to a C-compatible string
    /// or if the underlying SQLite call fails.
    pub fn query_row_and_then<T, E, P, F>(&self, sql: &str, params: P, f: F) -> result::Result<T, E>
    where
        P: IntoIterator,
        P::Item: ToSql,
        F: FnOnce(&Row<'_>) -> result::Result<T, E>,
        E: convert::From<Error>,
    {
        let mut stmt = self.prepare(sql)?;
        let mut rows = stmt.query(params)?;

        rows.get_expected_row().map_err(E::from).and_then(|r| f(&r))
    }

    /// Prepare a SQL statement for execution.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// fn insert_new_people(conn: &Connection) -> Result<()> {
    ///     let mut stmt = conn.prepare("INSERT INTO People (name) VALUES (?)")?;
    ///     stmt.execute(&["Joe Smith"])?;
    ///     stmt.execute(&["Bob Jones"])?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if `sql` cannot be converted to a C-compatible string
    /// or if the underlying SQLite call fails.
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        self.db.borrow_mut().prepare(self, sql)
    }

    /// Close the SQLite connection.
    ///
    /// This is functionally equivalent to the `Drop` implementation for
    /// `Connection` except that on failure, it returns an error and the
    /// connection itself (presumably so closing can be attempted again).
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    pub fn close(self) -> std::result::Result<(), (Connection, Error)> {
        self.flush_prepared_statement_cache();
        let r = self.db.borrow_mut().close();
        r.map_err(move |err| (self, err))
    }

    /// Enable loading of SQLite extensions. Strongly consider using
    /// `LoadExtensionGuard` instead of this function.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # use std::path::{Path};
    /// fn load_my_extension(conn: &Connection) -> Result<()> {
    ///     conn.load_extension_enable()?;
    ///     conn.load_extension(Path::new("my_sqlite_extension"), None)?;
    ///     conn.load_extension_disable()
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[cfg(feature = "load_extension")]
    pub fn load_extension_enable(&self) -> Result<()> {
        self.db.borrow_mut().enable_load_extension(1)
    }

    /// Disable loading of SQLite extensions.
    ///
    /// See `load_extension_enable` for an example.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[cfg(feature = "load_extension")]
    pub fn load_extension_disable(&self) -> Result<()> {
        self.db.borrow_mut().enable_load_extension(0)
    }

    /// Load the SQLite extension at `dylib_path`. `dylib_path` is passed
    /// through to `sqlite3_load_extension`, which may attempt OS-specific
    /// modifications if the file cannot be loaded directly.
    ///
    /// If `entry_point` is `None`, SQLite will attempt to find the entry
    /// point. If it is not `None`, the entry point will be passed through
    /// to `sqlite3_load_extension`.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result, LoadExtensionGuard};
    /// # use std::path::{Path};
    /// fn load_my_extension(conn: &Connection) -> Result<()> {
    ///     let _guard = LoadExtensionGuard::new(conn)?;
    ///
    ///     conn.load_extension("my_sqlite_extension", None)
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[cfg(feature = "load_extension")]
    pub fn load_extension<P: AsRef<Path>>(
        &self,
        dylib_path: P,
        entry_point: Option<&str>,
    ) -> Result<()> {
        self.db
            .borrow_mut()
            .load_extension(dylib_path.as_ref(), entry_point)
    }

    /// Get access to the underlying SQLite database connection handle.
    ///
    /// # Warning
    ///
    /// You should not need to use this function. If you do need to, please
    /// [open an issue on the rusqlite repository](https://github.com/jgallagher/rusqlite/issues) and describe
    /// your use case. This function is unsafe because it gives you raw access
    /// to the SQLite connection, and what you do with it could impact the
    /// safety of this `Connection`.
    pub unsafe fn handle(&self) -> *mut ffi::sqlite3 {
        self.db.borrow().db()
    }

    /// Create a `Connection` from a raw handle.
    ///
    /// The underlying SQLite database connection handle will not be closed when
    /// the returned connection is dropped/closed.
    pub unsafe fn from_handle(db: *mut ffi::sqlite3) -> Result<Connection> {
        let db_path = db_filename(db);
        let db = InnerConnection::new(db, false);
        Ok(Connection {
            db: RefCell::new(db),
            cache: StatementCache::with_capacity(STATEMENT_CACHE_DEFAULT_CAPACITY),
            path: db_path,
        })
    }

    /// Get access to a handle that can be used to interrupt long running
    /// queries from another thread.
    pub fn get_interrupt_handle(&self) -> InterruptHandle {
        self.db.borrow().get_interrupt_handle()
    }

    fn decode_result(&self, code: c_int) -> Result<()> {
        self.db.borrow_mut().decode_result(code)
    }

    /// Return the number of rows modified, inserted or deleted by the most
    /// recently completed INSERT, UPDATE or DELETE statement on the database
    /// connection.
    fn changes(&self) -> usize {
        self.db.borrow_mut().changes()
    }

    /// Test for auto-commit mode.
    /// Autocommit mode is on by default.
    pub fn is_autocommit(&self) -> bool {
        self.db.borrow().is_autocommit()
    }

    /// Determine if all associated prepared statements have been reset.
    #[cfg(feature = "bundled")]
    pub fn is_busy(&self) -> bool {
        self.db.borrow().is_busy()
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("path", &self.path)
            .finish()
    }
}

bitflags! {
    #[doc = "Flags for opening SQLite database connections."]
    #[doc = "See [sqlite3_open_v2](http://www.sqlite.org/c3ref/open.html) for details."]
    #[repr(C)]
    pub struct OpenFlags: ::std::os::raw::c_int {
        const SQLITE_OPEN_READ_ONLY     = ffi::SQLITE_OPEN_READONLY;
        const SQLITE_OPEN_READ_WRITE    = ffi::SQLITE_OPEN_READWRITE;
        const SQLITE_OPEN_CREATE        = ffi::SQLITE_OPEN_CREATE;
        const SQLITE_OPEN_URI           = 0x0000_0040;
        const SQLITE_OPEN_MEMORY        = 0x0000_0080;
        const SQLITE_OPEN_NO_MUTEX      = ffi::SQLITE_OPEN_NOMUTEX;
        const SQLITE_OPEN_FULL_MUTEX    = ffi::SQLITE_OPEN_FULLMUTEX;
        const SQLITE_OPEN_SHARED_CACHE  = 0x0002_0000;
        const SQLITE_OPEN_PRIVATE_CACHE = 0x0004_0000;
    }
}

impl Default for OpenFlags {
    fn default() -> OpenFlags {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI
    }
}

/// rusqlite's check for a safe SQLite threading mode requires SQLite 3.7.0 or
/// later. If you are running against a SQLite older than that, rusqlite
/// attempts to ensure safety by performing configuration and initialization of
/// SQLite itself the first time you
/// attempt to open a connection. By default, rusqlite panics if that
/// initialization fails, since that could mean SQLite has been initialized in
/// single-thread mode.
///
/// If you are encountering that panic _and_ can ensure that SQLite has been
/// initialized in either multi-thread or serialized mode, call this function
/// prior to attempting to open a connection and rusqlite's initialization
/// process will by skipped. This
/// function is unsafe because if you call it and SQLite has actually been
/// configured to run in single-thread mode,
/// you may enounter memory errors or data corruption or any number of terrible
/// things that should not be possible when you're using Rust.
pub unsafe fn bypass_sqlite_initialization() {
    BYPASS_SQLITE_INIT.store(true, Ordering::Relaxed);
}

/// rusqlite performs a one-time check that the runtime SQLite version is at
/// least as new as the version of SQLite found when rusqlite was built.
/// Bypassing this check may be dangerous; e.g., if you use features of SQLite
/// that are not present in the runtime
/// version. If you are sure the runtime version is compatible with the
/// build-time version for your usage, you can bypass the version check by
/// calling this function before
/// your first connection attempt.
pub unsafe fn bypass_sqlite_version_check() {
    #[cfg(not(feature = "bundled"))]
    inner_connection::BYPASS_VERSION_CHECK.store(true, Ordering::Relaxed);
}

/// Allows interrupting a long-running computation.
pub struct InterruptHandle {
    db_lock: Arc<Mutex<*mut ffi::sqlite3>>,
}

unsafe impl Send for InterruptHandle {}
unsafe impl Sync for InterruptHandle {}

impl InterruptHandle {
    /// Interrupt the query currently executing on another thread. This will
    /// cause that query to fail with a `SQLITE3_INTERRUPT` error.
    pub fn interrupt(&self) {
        let db_handle = self.db_lock.lock().unwrap();
        if !db_handle.is_null() {
            unsafe { ffi::sqlite3_interrupt(*db_handle) }
        }
    }
}

#[cfg(feature = "bundled")] // 3.7.10
unsafe fn db_filename(db: *mut ffi::sqlite3) -> Option<PathBuf> {
    let db_name = DatabaseName::Main.to_cstring().unwrap();
    let db_filename = ffi::sqlite3_db_filename(db, db_name.as_ptr());
    if db_filename.is_null() {
        None
    } else {
        CStr::from_ptr(db_filename).to_str().ok().map(PathBuf::from)
    }
}
#[cfg(not(feature = "bundled"))]
unsafe fn db_filename(_: *mut ffi::sqlite3) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod test {
    use self::tempdir::TempDir;
    pub use super::*;
    use crate::ffi;
    use fallible_iterator::FallibleIterator;
    pub use std::error::Error as StdError;
    pub use std::fmt;
    use tempdir;

    // this function is never called, but is still type checked; in
    // particular, calls with specific instantiations will require
    // that those types are `Send`.
    #[allow(dead_code, unconditional_recursion)]
    fn ensure_send<T: Send>() {
        ensure_send::<Connection>();
        ensure_send::<InterruptHandle>();
    }

    #[allow(dead_code, unconditional_recursion)]
    fn ensure_sync<T: Sync>() {
        ensure_sync::<InterruptHandle>();
    }

    pub fn checked_memory_handle() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn test_concurrent_transactions_busy_commit() {
        use std::time::Duration;
        let tmp = TempDir::new("locked").unwrap();
        let path = tmp.path().join("transactions.db3");

        Connection::open(&path)
            .expect("create temp db")
            .execute_batch(
                "
            BEGIN; CREATE TABLE foo(x INTEGER);
            INSERT INTO foo VALUES(42); END;",
            )
            .expect("create temp db");

        let mut db1 = Connection::open(&path).unwrap();
        let mut db2 = Connection::open(&path).unwrap();

        db1.busy_timeout(Duration::from_millis(0)).unwrap();
        db2.busy_timeout(Duration::from_millis(0)).unwrap();

        {
            let tx1 = db1.transaction().unwrap();
            let tx2 = db2.transaction().unwrap();

            // SELECT first makes sqlite lock with a shared lock
            tx1.query_row("SELECT x FROM foo LIMIT 1", NO_PARAMS, |_| Ok(()))
                .unwrap();
            tx2.query_row("SELECT x FROM foo LIMIT 1", NO_PARAMS, |_| Ok(()))
                .unwrap();

            tx1.execute("INSERT INTO foo VALUES(?1)", &[1]).unwrap();
            let _ = tx2.execute("INSERT INTO foo VALUES(?1)", &[2]);

            let _ = tx1.commit();
            let _ = tx2.commit();
        }

        let _ = db1
            .transaction()
            .expect("commit should have closed transaction");
        let _ = db2
            .transaction()
            .expect("commit should have closed transaction");
    }

    #[test]
    fn test_persistence() {
        let temp_dir = TempDir::new("test_open_file").unwrap();
        let path = temp_dir.path().join("test.db3");

        {
            let db = Connection::open(&path).unwrap();
            let sql = "BEGIN;
                   CREATE TABLE foo(x INTEGER);
                   INSERT INTO foo VALUES(42);
                   END;";
            db.execute_batch(sql).unwrap();
        }

        let path_string = path.to_str().unwrap();
        let db = Connection::open(&path_string).unwrap();
        let the_answer: Result<i64> = db.query_row("SELECT x FROM foo", NO_PARAMS, |r| r.get(0));

        assert_eq!(42i64, the_answer.unwrap());
    }

    #[test]
    fn test_open() {
        assert!(Connection::open_in_memory().is_ok());

        let db = checked_memory_handle();
        assert!(db.close().is_ok());
    }

    #[test]
    fn test_close_retry() {
        let db = checked_memory_handle();

        // force the DB to be busy by preparing a statement; this must be done at the
        // FFI level to allow us to call .close() without dropping the prepared
        // statement first.
        let raw_stmt = {
            use super::str_to_cstring;
            use std::mem;
            use std::os::raw::c_int;
            use std::ptr;

            let raw_db = db.db.borrow_mut().db;
            let sql = "SELECT 1";
            let mut raw_stmt: *mut ffi::sqlite3_stmt = unsafe { mem::uninitialized() };
            let rc = unsafe {
                ffi::sqlite3_prepare_v2(
                    raw_db,
                    str_to_cstring(sql).unwrap().as_ptr(),
                    (sql.len() + 1) as c_int,
                    &mut raw_stmt,
                    ptr::null_mut(),
                )
            };
            assert_eq!(rc, ffi::SQLITE_OK);
            raw_stmt
        };

        // now that we have an open statement, trying (and retrying) to close should
        // fail.
        let (db, _) = db.close().unwrap_err();
        let (db, _) = db.close().unwrap_err();
        let (db, _) = db.close().unwrap_err();

        // finalize the open statement so a final close will succeed
        assert_eq!(ffi::SQLITE_OK, unsafe { ffi::sqlite3_finalize(raw_stmt) });

        db.close().unwrap();
    }

    #[test]
    fn test_open_with_flags() {
        for bad_flags in &[
            OpenFlags::empty(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_READ_WRITE,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_CREATE,
        ] {
            assert!(Connection::open_in_memory_with_flags(*bad_flags).is_err());
        }
    }

    #[test]
    fn test_execute_batch() {
        let db = checked_memory_handle();
        let sql = "BEGIN;
                   CREATE TABLE foo(x INTEGER);
                   INSERT INTO foo VALUES(1);
                   INSERT INTO foo VALUES(2);
                   INSERT INTO foo VALUES(3);
                   INSERT INTO foo VALUES(4);
                   END;";
        db.execute_batch(sql).unwrap();

        db.execute_batch("UPDATE foo SET x = 3 WHERE x < 3")
            .unwrap();

        assert!(db.execute_batch("INVALID SQL").is_err());
    }

    #[test]
    fn test_execute() {
        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(x INTEGER)").unwrap();

        assert_eq!(
            1,
            db.execute("INSERT INTO foo(x) VALUES (?)", &[1i32])
                .unwrap()
        );
        assert_eq!(
            1,
            db.execute("INSERT INTO foo(x) VALUES (?)", &[2i32])
                .unwrap()
        );

        assert_eq!(
            3i32,
            db.query_row::<i32, _, _>("SELECT SUM(x) FROM foo", NO_PARAMS, |r| r.get(0))
                .unwrap()
        );
    }

    #[test]
    fn test_execute_select() {
        let db = checked_memory_handle();
        let err = db.execute("SELECT 1 WHERE 1 < ?", &[1i32]).unwrap_err();
        if err != Error::ExecuteReturnedResults {
            panic!("Unexpected error: {}", err);
        }
    }

    #[test]
    fn test_prepare_column_names() {
        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(x INTEGER);").unwrap();

        let stmt = db.prepare("SELECT * FROM foo").unwrap();
        assert_eq!(stmt.column_count(), 1);
        assert_eq!(stmt.column_names(), vec!["x"]);

        let stmt = db.prepare("SELECT x AS a, x AS b FROM foo").unwrap();
        assert_eq!(stmt.column_count(), 2);
        assert_eq!(stmt.column_names(), vec!["a", "b"]);
    }

    #[test]
    fn test_prepare_execute() {
        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(x INTEGER);").unwrap();

        let mut insert_stmt = db.prepare("INSERT INTO foo(x) VALUES(?)").unwrap();
        assert_eq!(insert_stmt.execute(&[1i32]).unwrap(), 1);
        assert_eq!(insert_stmt.execute(&[2i32]).unwrap(), 1);
        assert_eq!(insert_stmt.execute(&[3i32]).unwrap(), 1);

        assert_eq!(insert_stmt.execute(&["hello".to_string()]).unwrap(), 1);
        assert_eq!(insert_stmt.execute(&["goodbye".to_string()]).unwrap(), 1);
        assert_eq!(insert_stmt.execute(&[types::Null]).unwrap(), 1);

        let mut update_stmt = db.prepare("UPDATE foo SET x=? WHERE x<?").unwrap();
        assert_eq!(update_stmt.execute(&[3i32, 3i32]).unwrap(), 2);
        assert_eq!(update_stmt.execute(&[3i32, 3i32]).unwrap(), 0);
        assert_eq!(update_stmt.execute(&[8i32, 8i32]).unwrap(), 3);
    }

    #[test]
    fn test_prepare_query() {
        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(x INTEGER);").unwrap();

        let mut insert_stmt = db.prepare("INSERT INTO foo(x) VALUES(?)").unwrap();
        assert_eq!(insert_stmt.execute(&[1i32]).unwrap(), 1);
        assert_eq!(insert_stmt.execute(&[2i32]).unwrap(), 1);
        assert_eq!(insert_stmt.execute(&[3i32]).unwrap(), 1);

        let mut query = db
            .prepare("SELECT x FROM foo WHERE x < ? ORDER BY x DESC")
            .unwrap();
        {
            let mut rows = query.query(&[4i32]).unwrap();
            let mut v = Vec::<i32>::new();

            while let Some(row) = rows.next().unwrap() {
                v.push(row.get(0).unwrap());
            }

            assert_eq!(v, [3i32, 2, 1]);
        }

        {
            let mut rows = query.query(&[3i32]).unwrap();
            let mut v = Vec::<i32>::new();

            while let Some(row) = rows.next().unwrap() {
                v.push(row.get(0).unwrap());
            }

            assert_eq!(v, [2i32, 1]);
        }
    }

    #[test]
    fn test_query_map() {
        let db = checked_memory_handle();
        let sql = "BEGIN;
                   CREATE TABLE foo(x INTEGER, y TEXT);
                   INSERT INTO foo VALUES(4, \"hello\");
                   INSERT INTO foo VALUES(3, \", \");
                   INSERT INTO foo VALUES(2, \"world\");
                   INSERT INTO foo VALUES(1, \"!\");
                   END;";
        db.execute_batch(sql).unwrap();

        let mut query = db.prepare("SELECT x, y FROM foo ORDER BY x DESC").unwrap();
        let results: Result<Vec<String>> = query
            .query(NO_PARAMS)
            .unwrap()
            .map(|row| row.get(1))
            .collect();

        assert_eq!(results.unwrap().concat(), "hello, world!");
    }

    #[test]
    fn test_query_row() {
        let db = checked_memory_handle();
        let sql = "BEGIN;
                   CREATE TABLE foo(x INTEGER);
                   INSERT INTO foo VALUES(1);
                   INSERT INTO foo VALUES(2);
                   INSERT INTO foo VALUES(3);
                   INSERT INTO foo VALUES(4);
                   END;";
        db.execute_batch(sql).unwrap();

        assert_eq!(
            10i64,
            db.query_row::<i64, _, _>("SELECT SUM(x) FROM foo", NO_PARAMS, |r| r.get(0))
                .unwrap()
        );

        let result: Result<i64> =
            db.query_row("SELECT x FROM foo WHERE x > 5", NO_PARAMS, |r| r.get(0));
        match result.unwrap_err() {
            Error::QueryReturnedNoRows => (),
            err => panic!("Unexpected error {}", err),
        }

        let bad_query_result = db.query_row("NOT A PROPER QUERY; test123", NO_PARAMS, |_| Ok(()));

        assert!(bad_query_result.is_err());
    }

    #[test]
    fn test_optional() {
        let db = checked_memory_handle();

        let result: Result<i64> = db.query_row("SELECT 1 WHERE 0 <> 0", NO_PARAMS, |r| r.get(0));
        let result = result.optional();
        match result.unwrap() {
            None => (),
            _ => panic!("Unexpected result"),
        }

        let result: Result<i64> = db.query_row("SELECT 1 WHERE 0 == 0", NO_PARAMS, |r| r.get(0));
        let result = result.optional();
        match result.unwrap() {
            Some(1) => (),
            _ => panic!("Unexpected result"),
        }

        let bad_query_result: Result<i64> =
            db.query_row("NOT A PROPER QUERY", NO_PARAMS, |r| r.get(0));
        let bad_query_result = bad_query_result.optional();
        assert!(bad_query_result.is_err());
    }

    #[test]
    fn test_pragma_query_row() {
        let db = checked_memory_handle();

        assert_eq!(
            "memory",
            db.query_row::<String, _, _>("PRAGMA journal_mode", NO_PARAMS, |r| r.get(0))
                .unwrap()
        );
        assert_eq!(
            "off",
            db.query_row::<String, _, _>("PRAGMA journal_mode=off", NO_PARAMS, |r| r.get(0))
                .unwrap()
        );
    }

    #[test]
    fn test_prepare_failures() {
        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(x INTEGER);").unwrap();

        let err = db.prepare("SELECT * FROM does_not_exist").unwrap_err();
        assert!(format!("{}", err).contains("does_not_exist"));
    }

    #[test]
    fn test_last_insert_rowid() {
        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(x INTEGER PRIMARY KEY)")
            .unwrap();
        db.execute_batch("INSERT INTO foo DEFAULT VALUES").unwrap();

        assert_eq!(db.last_insert_rowid(), 1);

        let mut stmt = db.prepare("INSERT INTO foo DEFAULT VALUES").unwrap();
        for _ in 0i32..9 {
            stmt.execute(NO_PARAMS).unwrap();
        }
        assert_eq!(db.last_insert_rowid(), 10);
    }

    #[test]
    fn test_is_autocommit() {
        let db = checked_memory_handle();
        assert!(
            db.is_autocommit(),
            "autocommit expected to be active by default"
        );
    }

    #[test]
    #[cfg(feature = "bundled")]
    fn test_is_busy() {
        let db = checked_memory_handle();
        assert!(!db.is_busy());
        let mut stmt = db.prepare("PRAGMA schema_version").unwrap();
        assert!(!db.is_busy());
        {
            let mut rows = stmt.query(NO_PARAMS).unwrap();
            assert!(!db.is_busy());
            let row = rows.next().unwrap();
            assert!(db.is_busy());
            assert!(row.is_some());
        }
        assert!(!db.is_busy());
    }

    #[test]
    fn test_statement_debugging() {
        let db = checked_memory_handle();
        let query = "SELECT 12345";
        let stmt = db.prepare(query).unwrap();

        assert!(format!("{:?}", stmt).contains(query));
    }

    #[test]
    fn test_notnull_constraint_error() {
        // extended error codes for constraints were added in SQLite 3.7.16; if we're
        // running on our bundled version, we know the extended error code exists.
        #[cfg(feature = "bundled")]
        fn check_extended_code(extended_code: c_int) {
            assert_eq!(extended_code, ffi::SQLITE_CONSTRAINT_NOTNULL);
        }
        #[cfg(not(feature = "bundled"))]
        fn check_extended_code(_extended_code: c_int) {}

        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(x NOT NULL)").unwrap();

        let result = db.execute("INSERT INTO foo (x) VALUES (NULL)", NO_PARAMS);
        assert!(result.is_err());

        match result.unwrap_err() {
            Error::SqliteFailure(err, _) => {
                assert_eq!(err.code, ErrorCode::ConstraintViolation);
                check_extended_code(err.extended_code);
            }
            err => panic!("Unexpected error {}", err),
        }
    }

    #[test]
    fn test_version_string() {
        let n = version_number();
        let major = n / 1_000_000;
        let minor = (n % 1_000_000) / 1_000;
        let patch = n % 1_000;

        assert!(version().contains(&format!("{}.{}.{}", major, minor, patch)));
    }

    #[test]
    #[cfg(feature = "functions")]
    fn test_interrupt() {
        let db = checked_memory_handle();

        let interrupt_handle = db.get_interrupt_handle();

        db.create_scalar_function("interrupt", 0, false, move |_| {
            interrupt_handle.interrupt();
            Ok(0)
        })
        .unwrap();

        let mut stmt = db
            .prepare("SELECT interrupt() FROM (SELECT 1 UNION SELECT 2 UNION SELECT 3)")
            .unwrap();

        let result: Result<Vec<i32>> = stmt.query(NO_PARAMS).unwrap().map(|r| r.get(0)).collect();

        match result.unwrap_err() {
            Error::SqliteFailure(err, _) => {
                assert_eq!(err.code, ErrorCode::OperationInterrupted);
                return;
            }
            err => {
                panic!("Unexpected error {}", err);
            }
        }
    }

    #[test]
    fn test_interrupt_close() {
        let db = checked_memory_handle();
        let handle = db.get_interrupt_handle();
        handle.interrupt();
        db.close().unwrap();
        handle.interrupt();

        // Look at it's internals to see if we cleared it out properly.
        let db_guard = handle.db_lock.lock().unwrap();
        assert!(db_guard.is_null());
        // It would be nice to test that we properly handle close/interrupt
        // running at the same time, but it seems impossible to do with any
        // degree of reliability.
    }

    #[test]
    fn test_get_raw() {
        let db = checked_memory_handle();
        db.execute_batch("CREATE TABLE foo(i, x);").unwrap();
        let vals = ["foobar", "1234", "qwerty"];
        let mut insert_stmt = db.prepare("INSERT INTO foo(i, x) VALUES(?, ?)").unwrap();
        for (i, v) in vals.iter().enumerate() {
            let i_to_insert = i as i64;
            assert_eq!(insert_stmt.execute(params![i_to_insert, v]).unwrap(), 1);
        }

        let mut query = db.prepare("SELECT i, x FROM foo").unwrap();
        let mut rows = query.query(NO_PARAMS).unwrap();

        while let Some(row) = rows.next().unwrap() {
            let i = row.get_raw(0).as_i64().unwrap();
            let expect = vals[i as usize];
            let x = row.get_raw("x").as_str().unwrap();
            assert_eq!(x, expect);
        }
    }

    #[test]
    fn test_from_handle() {
        let db = checked_memory_handle();
        let handle = unsafe { db.handle() };
        {
            let db = unsafe { Connection::from_handle(handle) }.unwrap();
            db.execute_batch("PRAGMA VACUUM").unwrap();
        }
        db.close().unwrap();
    }

    mod query_and_then_tests {

        use super::*;

        #[derive(Debug)]
        enum CustomError {
            SomeError,
            Sqlite(Error),
        }

        impl fmt::Display for CustomError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> ::std::result::Result<(), fmt::Error> {
                match *self {
                    CustomError::SomeError => write!(f, "{}", self.description()),
                    CustomError::Sqlite(ref se) => write!(f, "{}: {}", self.description(), se),
                }
            }
        }

        impl StdError for CustomError {
            fn description(&self) -> &str {
                "my custom error"
            }

            fn cause(&self) -> Option<&dyn StdError> {
                match *self {
                    CustomError::SomeError => None,
                    CustomError::Sqlite(ref se) => Some(se),
                }
            }
        }

        impl From<Error> for CustomError {
            fn from(se: Error) -> CustomError {
                CustomError::Sqlite(se)
            }
        }

        type CustomResult<T> = ::std::result::Result<T, CustomError>;

        #[test]
        fn test_query_and_then() {
            let db = checked_memory_handle();
            let sql = "BEGIN;
                       CREATE TABLE foo(x INTEGER, y TEXT);
                       INSERT INTO foo VALUES(4, \"hello\");
                       INSERT INTO foo VALUES(3, \", \");
                       INSERT INTO foo VALUES(2, \"world\");
                       INSERT INTO foo VALUES(1, \"!\");
                       END;";
            db.execute_batch(sql).unwrap();

            let mut query = db.prepare("SELECT x, y FROM foo ORDER BY x DESC").unwrap();
            let results: Result<Vec<String>> = query
                .query_and_then(NO_PARAMS, |row| row.get(1))
                .unwrap()
                .collect();

            assert_eq!(results.unwrap().concat(), "hello, world!");
        }

        #[test]
        fn test_query_and_then_fails() {
            let db = checked_memory_handle();
            let sql = "BEGIN;
                       CREATE TABLE foo(x INTEGER, y TEXT);
                       INSERT INTO foo VALUES(4, \"hello\");
                       INSERT INTO foo VALUES(3, \", \");
                       INSERT INTO foo VALUES(2, \"world\");
                       INSERT INTO foo VALUES(1, \"!\");
                       END;";
            db.execute_batch(sql).unwrap();

            let mut query = db.prepare("SELECT x, y FROM foo ORDER BY x DESC").unwrap();
            let bad_type: Result<Vec<f64>> = query
                .query_and_then(NO_PARAMS, |row| row.get(1))
                .unwrap()
                .collect();

            match bad_type.unwrap_err() {
                Error::InvalidColumnType(_, _) => (),
                err => panic!("Unexpected error {}", err),
            }

            let bad_idx: Result<Vec<String>> = query
                .query_and_then(NO_PARAMS, |row| row.get(3))
                .unwrap()
                .collect();

            match bad_idx.unwrap_err() {
                Error::InvalidColumnIndex(_) => (),
                err => panic!("Unexpected error {}", err),
            }
        }

        #[test]
        fn test_query_and_then_custom_error() {
            let db = checked_memory_handle();
            let sql = "BEGIN;
                       CREATE TABLE foo(x INTEGER, y TEXT);
                       INSERT INTO foo VALUES(4, \"hello\");
                       INSERT INTO foo VALUES(3, \", \");
                       INSERT INTO foo VALUES(2, \"world\");
                       INSERT INTO foo VALUES(1, \"!\");
                       END;";
            db.execute_batch(sql).unwrap();

            let mut query = db.prepare("SELECT x, y FROM foo ORDER BY x DESC").unwrap();
            let results: CustomResult<Vec<String>> = query
                .query_and_then(NO_PARAMS, |row| row.get(1).map_err(CustomError::Sqlite))
                .unwrap()
                .collect();

            assert_eq!(results.unwrap().concat(), "hello, world!");
        }

        #[test]
        fn test_query_and_then_custom_error_fails() {
            let db = checked_memory_handle();
            let sql = "BEGIN;
                       CREATE TABLE foo(x INTEGER, y TEXT);
                       INSERT INTO foo VALUES(4, \"hello\");
                       INSERT INTO foo VALUES(3, \", \");
                       INSERT INTO foo VALUES(2, \"world\");
                       INSERT INTO foo VALUES(1, \"!\");
                       END;";
            db.execute_batch(sql).unwrap();

            let mut query = db.prepare("SELECT x, y FROM foo ORDER BY x DESC").unwrap();
            let bad_type: CustomResult<Vec<f64>> = query
                .query_and_then(NO_PARAMS, |row| row.get(1).map_err(CustomError::Sqlite))
                .unwrap()
                .collect();

            match bad_type.unwrap_err() {
                CustomError::Sqlite(Error::InvalidColumnType(_, _)) => (),
                err => panic!("Unexpected error {}", err),
            }

            let bad_idx: CustomResult<Vec<String>> = query
                .query_and_then(NO_PARAMS, |row| row.get(3).map_err(CustomError::Sqlite))
                .unwrap()
                .collect();

            match bad_idx.unwrap_err() {
                CustomError::Sqlite(Error::InvalidColumnIndex(_)) => (),
                err => panic!("Unexpected error {}", err),
            }

            let non_sqlite_err: CustomResult<Vec<String>> = query
                .query_and_then(NO_PARAMS, |_| Err(CustomError::SomeError))
                .unwrap()
                .collect();

            match non_sqlite_err.unwrap_err() {
                CustomError::SomeError => (),
                err => panic!("Unexpected error {}", err),
            }
        }

        #[test]
        fn test_query_row_and_then_custom_error() {
            let db = checked_memory_handle();
            let sql = "BEGIN;
                       CREATE TABLE foo(x INTEGER, y TEXT);
                       INSERT INTO foo VALUES(4, \"hello\");
                       END;";
            db.execute_batch(sql).unwrap();

            let query = "SELECT x, y FROM foo ORDER BY x DESC";
            let results: CustomResult<String> = db.query_row_and_then(query, NO_PARAMS, |row| {
                row.get(1).map_err(CustomError::Sqlite)
            });

            assert_eq!(results.unwrap(), "hello");
        }

        #[test]
        fn test_query_row_and_then_custom_error_fails() {
            let db = checked_memory_handle();
            let sql = "BEGIN;
                       CREATE TABLE foo(x INTEGER, y TEXT);
                       INSERT INTO foo VALUES(4, \"hello\");
                       END;";
            db.execute_batch(sql).unwrap();

            let query = "SELECT x, y FROM foo ORDER BY x DESC";
            let bad_type: CustomResult<f64> = db.query_row_and_then(query, NO_PARAMS, |row| {
                row.get(1).map_err(CustomError::Sqlite)
            });

            match bad_type.unwrap_err() {
                CustomError::Sqlite(Error::InvalidColumnType(_, _)) => (),
                err => panic!("Unexpected error {}", err),
            }

            let bad_idx: CustomResult<String> = db.query_row_and_then(query, NO_PARAMS, |row| {
                row.get(3).map_err(CustomError::Sqlite)
            });

            match bad_idx.unwrap_err() {
                CustomError::Sqlite(Error::InvalidColumnIndex(_)) => (),
                err => panic!("Unexpected error {}", err),
            }

            let non_sqlite_err: CustomResult<String> =
                db.query_row_and_then(query, NO_PARAMS, |_| Err(CustomError::SomeError));

            match non_sqlite_err.unwrap_err() {
                CustomError::SomeError => (),
                err => panic!("Unexpected error {}", err),
            }
        }

        #[test]
        fn test_dynamic() {
            let db = checked_memory_handle();
            let sql = "BEGIN;
                       CREATE TABLE foo(x INTEGER, y TEXT);
                       INSERT INTO foo VALUES(4, \"hello\");
                       END;";
            db.execute_batch(sql).unwrap();

            db.query_row("SELECT * FROM foo", params![], |r| {
                assert_eq!(2, r.column_count());
                Ok(())
            })
            .unwrap();
        }
        #[test]
        fn test_dyn_box() {
            let db = checked_memory_handle();
            db.execute_batch("CREATE TABLE foo(x INTEGER);").unwrap();
            let b: Box<dyn ToSql> = Box::new(5);
            db.execute("INSERT INTO foo VALUES(?)", &[b]).unwrap();
            db.query_row("SELECT x FROM foo", params![], |r| {
                assert_eq!(5, r.get_unwrap::<_, i32>(0));
                Ok(())
            })
            .unwrap();
        }
    }
}
