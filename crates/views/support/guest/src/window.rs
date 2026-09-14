//! Window effects use the host's scoped request channel.
use crate::{host, wire};

/// Completion acknowledges submission to the native runtime, not OS application.
/// Rejections are explicit host responses and appear in the guest log.
pub fn perform<M: 'static>(command: wire::WindowCommand) -> crate::Task<M> {
    crate::Task::future(async move {
        let result = match command.validate() {
            Ok(()) => host::request("host.window", &wire::encode(&command)).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            host::log(format!("host.window: {error}"));
        }
    })
    .discard()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, Driver};
    struct WindowApp(bool);
    impl App for WindowApp {
        type Message = bool;
        fn boot() -> (Self, crate::Task<bool>) {
            (
                Self(false),
                perform(wire::WindowCommand::Focus).chain(crate::Task::done(true)),
            )
        }
        fn view(&self) -> wire::Node {
            wire::Node::empty()
        }
        fn update(&mut self, submitted: bool) -> crate::Task<bool> {
            self.0 = submitted;
            crate::Task::none()
        }
        fn subscription(&self) -> crate::Subscription<bool> {
            crate::Subscription::none()
        }
    }
    #[test]
    fn window_effect_waits_for_response_and_reports_rejection() {
        let mut driver = Driver::<WindowApp>::new();
        let frame = driver.tick(vec![]);
        let [request] = frame.requests.as_slice() else {
            panic!("one request: {:?}", frame.requests)
        };
        assert_eq!(request.kind, "host.window");
        assert_eq!(
            wire::WindowCommand::decode(&request.payload).unwrap(),
            wire::WindowCommand::Focus
        );
        assert!(!driver.app.0, "the chain must wait for host submission");
        let frame = driver.tick(vec![wire::Event::Response {
            id: request.id,
            result: Err("RequestError: guest closed".into()),
            done: true,
        }]);
        assert!(driver.app.0, "an explicit refusal must settle the task");
        assert!(
            frame
                .requests
                .iter()
                .any(|request| request.kind == "host.log"
                    && String::from_utf8_lossy(&request.payload)
                        .contains("RequestError: guest closed"))
        );
    }
}
