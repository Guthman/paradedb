// Copyright (c) 2023-2026 ParadeDB, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

//! Hooks that Postgres calls on a thread other than the backend's main thread.
//!
//! pg_duckdb and pg_ducklake run Postgres code (SPI, `standard_planner`, the executor) on
//! DuckDB worker threads, one thread at a time under their own lock. Postgres then calls the
//! hooks that pg_search installs on that thread. pgrx does not allow FFI there: the first
//! `pg_sys::` call panics and the backend aborts, although the query does not use pg_search.
//!
//! On such a thread our hooks make no `pg_sys::` call. A hook that only adds paths returns at
//! once. A hook that must call the next hook or a `standard_*` function is registered as a
//! plain `extern "C-unwind"` function, not `#[pg_guard]`, so that a Postgres ERROR raised
//! below it longjmps across a frame with nothing to drop, to the handler of the extension
//! that owns the thread.

use pgrx::pg_sys;
use std::cell::Cell;

std::thread_local! {
    static IS_MAIN_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Called from `_PG_init`, which runs on the postmaster's main thread. A forked backend
/// inherits the mark on its only thread. Threads started later in the backend do not have it.
pub fn mark_main_thread() {
    IS_MAIN_THREAD.with(|t| t.set(true));
}

/// Returns false on a thread that another extension started in this backend.
#[inline]
pub fn is_main_thread() -> bool {
    IS_MAIN_THREAD.with(|t| t.get())
}

// The `standard_*` functions without pgrx's FFI wrapper, which panics off the main thread.
unsafe extern "C-unwind" {
    pub fn standard_planner(
        parse: *mut pg_sys::Query,
        query_string: *const ::core::ffi::c_char,
        cursor_options: ::core::ffi::c_int,
        bound_params: pg_sys::ParamListInfo,
    ) -> *mut pg_sys::PlannedStmt;

    // Used by the hooks in `fake_aminsertcleanup`, which only pg15 and pg16 build.
    #[cfg(not(any(feature = "pg17", feature = "pg18")))]
    pub fn standard_ExecutorRun(
        query_desc: *mut pg_sys::QueryDesc,
        direction: pg_sys::ScanDirection::Type,
        count: pg_sys::uint64,
        execute_once: bool,
    );

    #[cfg(not(any(feature = "pg17", feature = "pg18")))]
    pub fn standard_ExecutorFinish(query_desc: *mut pg_sys::QueryDesc);

    #[cfg(not(any(feature = "pg17", feature = "pg18")))]
    pub fn standard_ProcessUtility(
        pstmt: *mut pg_sys::PlannedStmt,
        query_string: *const ::core::ffi::c_char,
        read_only_tree: bool,
        context: pg_sys::ProcessUtilityContext::Type,
        params: pg_sys::ParamListInfo,
        query_env: *mut pg_sys::QueryEnvironment,
        dest: *mut pg_sys::DestReceiver,
        qc: *mut pg_sys::QueryCompletion,
    );
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::{pg_sys, pg_test};

    // Raw, like the functions above: pgrx's wrappers panic off the main thread.
    unsafe extern "C-unwind" {
        fn SPI_connect() -> ::core::ffi::c_int;
        fn SPI_execute(
            src: *const ::core::ffi::c_char,
            read_only: bool,
            tcount: ::core::ffi::c_long,
        ) -> ::core::ffi::c_int;
        fn SPI_finish() -> ::core::ffi::c_int;
        fn set_stack_base() -> pg_sys::pg_stack_base_t;
        fn restore_stack_base(base: pg_sys::pg_stack_base_t);
    }

    /// Does what pg_duckdb and pg_ducklake do: plan and run a query on another thread while
    /// the backend's main thread waits. The query scans a relation and aggregates, so the
    /// planner, rel-pathlist and upper-paths hooks all run on that thread.
    #[pg_test]
    fn query_on_foreign_thread() {
        let rc = std::thread::spawn(|| unsafe {
            // Postgres measures stack depth from the main thread's stack otherwise.
            let stack_base = set_stack_base();
            assert_eq!(SPI_connect(), pg_sys::SPI_OK_CONNECT as i32);
            let rc = SPI_execute(c"SELECT count(*) FROM pg_class".as_ptr(), true, 0);
            SPI_finish();
            restore_stack_base(stack_base);
            rc
        })
        .join()
        .expect("a pg_search hook panicked on a foreign thread");
        assert_eq!(rc, pg_sys::SPI_OK_SELECT as i32);
    }
}
