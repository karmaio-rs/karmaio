use std::io;

use karmaio::io::Stream;
use karmaio::runtime::{
    CancellationSource, CancellationToken, FutureExt, OperationCanceled, StreamExt, WithCancellation,
    is_operation_canceled, operation_canceled,
};

struct OneItem(bool);

impl Stream for OneItem {
    type Item = ();

    async fn next(&mut self) -> Option<Self::Item> {
        self.0.then(|| {
            self.0 = false;
        })
    }
}

#[test]
fn cancellation_api_is_available_from_runtime() -> io::Result<()> {
    let mut runtime = karmaio::Runtime::new()?;

    runtime.block_on(async {
        let source = CancellationSource::new();
        let token: CancellationToken = source.token();

        let wait = token.cancelled();
        source.cancel();
        wait.await;

        let future: WithCancellation<_> = std::future::ready(()).with_cancellation(token);
        future.await;

        let mut stream = OneItem(true).with_cancellation(token);
        assert_eq!(stream.next().await, Some(()));
        assert_eq!(stream.next().await, None);
    });

    let error = operation_canceled();
    assert!(is_operation_canceled(&error));
    assert!(error.get_ref().is_some_and(|source| source.is::<OperationCanceled>()));
    Ok(())
}
