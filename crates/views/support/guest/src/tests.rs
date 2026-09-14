use super::*;

struct Probe { key: u32, received: u32 }
impl App for Probe {
    type Message = ();
    fn boot() -> (Self, Task<()>) { (Self { key: 1, received: 0 }, Task::none()) }
    fn view(&self) -> wire::Node { wire::Node::empty() }
    fn update(&mut self, _: ()) -> Task<()> { self.received += 1; Task::none() }
    fn subscription(&self) -> Subscription<()> {
        Subscription::run_with(self.key, |key| host::subscribe("probe", &key.to_le_bytes()).map(|_| ()))
    }
}
impl SnapshotApp for Probe {
    fn snapshot(&self) -> Result<Vec<u8>, String> { Ok(self.received.to_le_bytes().to_vec()) }
    fn restore(bytes: &[u8]) -> Result<Self, String> {
        Ok(Self { key: 1, received: u32::from_le_bytes(bytes.try_into().map_err(|_| "invalid probe")?) })
    }
}

#[test]
fn subscriptions_retain_streams_and_cancel_changed_recipes() {
    let mut driver = Driver::<Probe>::new();
    let first = driver.tick(vec![]);
    let id = first.requests[0].id;
    assert!(driver.tick(vec![]).requests.is_empty());
    driver.tick(vec![wire::Event::Response { id, result: Ok(Vec::new()), done: false }]);
    assert_eq!(driver.app.received, 1);
    assert!(driver.snapshot().is_ok(), "subscriptions can be restored independently");
    driver.app.key = 2;
    let next = driver.tick(vec![]);
    assert_eq!(next.cancels, vec![id]);
    assert_eq!(next.requests.len(), 1);
}

#[test]
fn snapshot_refuses_unsettled_writes_and_failed_restore_keeps_old_driver() {
    let mut driver = Driver::<Probe>::new();
    driver.tick(vec![]);
    spawn(&mut driver.tasks, Task::future(async { let _ = host::request("write", &[]).await; }));
    let frame = driver.tick(vec![]);
    assert!(driver.snapshot().is_err());
    let write = frame.requests.iter().find(|request| request.kind == "write").unwrap();
    driver.tick(vec![wire::Event::Response { id: write.id, result: Ok(Vec::new()), done: true }]);
    let snapshot = driver.snapshot().unwrap();
    assert!(Driver::<Probe>::from_snapshot(&[], false).is_err());
    assert_eq!(driver.snapshot().unwrap(), snapshot);
}

#[test]
fn task_chaining_and_abort_preserve_effect_order() {
    let task = Task::done(Some(3)).and_then(|value| Task::done(value + 1)).chain(Task::done(5));
    assert_eq!(futures::executor::block_on(task.into_stream().collect::<Vec<_>>()), vec![4, 5]);
    let (task, handle) = Task::done(1).abortable();
    handle.abort();
    assert!(futures::executor::block_on(task.into_stream().collect::<Vec<_>>()).is_empty());
}
