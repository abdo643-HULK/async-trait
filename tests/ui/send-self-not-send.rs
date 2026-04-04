// Test that #[async_trait(Send)] emits a context-aware E0277 via
// #[diagnostic::on_unimplemented] when Self does not implement Send.
use async_trait::async_trait;

// A type that is explicitly not Send (raw pointer)
struct NotSend(*mut ());

#[async_trait(Send)]
trait MySendTrait {
    // A default method body causes `where Self: AsyncTraitSendSync + 'async_trait`
    // to be added; this fires the custom diagnostic when Self is not Send.
    async fn default_method(&self) {}
}

impl MySendTrait for NotSend {}

fn main() {
    let x = NotSend(std::ptr::null_mut());
    // Calling the method forces Rust to check `NotSend: AsyncTraitSendSync`.
    let _ = x.default_method();
}
