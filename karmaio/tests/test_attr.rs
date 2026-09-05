#![cfg(feature = "macros")]

// Verifies that #[karmaio::test] drives an async test on a karmaio runtime.
#[karmaio::test]
async fn block_on_runs_the_body() {
    let value = async { 2 * 21 }.await;
    assert_eq!(value, 42);
}

#[karmaio::test(blocking_threads = 8)]
async fn accepts_builder_args() {
    let value = async { 6 * 7 }.await;
    assert_eq!(value, 42);
}
