use pgrx::prelude::*;

::pgrx::pg_module_magic!(name, version);

#[pg_extern]
fn hello_pg_automerge() -> &'static str {
    "Hello, pg_automerge"
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_hello_pg_automerge() {
        assert_eq!("Hello, pg_automerge", crate::hello_pg_automerge());
    }

}

#[cfg(feature = "pg_bench")]
#[pg_schema]
mod benches {
    use pgrx::prelude::*;
    use pgrx_bench::{Bencher, black_box};

    #[pg_bench]
    fn bench_hello_pg_automerge(b: &mut Bencher) {
        b.iter(|| {
            black_box(crate::hello_pg_automerge());
        });
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests
        vec![]
    }
}
