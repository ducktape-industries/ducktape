//! Cooperative guest work. Only messages leave a task; host effects use requests.
use std::future::Future;
use futures::{Stream, StreamExt};

pub type BoxStream<T> = futures::stream::LocalBoxStream<'static, T>;

pub struct Task<T>(pub(crate) Option<BoxStream<T>>);

impl<T: 'static> Task<T> {
    pub fn none() -> Self { Self(None) }
    pub fn done(value: T) -> Self { Self::future(std::future::ready(value)) }
    pub fn future(future: impl Future<Output = T> + 'static) -> Self {
        Self::stream(futures::stream::once(future))
    }
    pub fn stream(stream: impl Stream<Item = T> + 'static) -> Self {
        Self(Some(stream.boxed_local()))
    }
    pub fn run<A: 'static>(stream: impl Stream<Item = A> + 'static, map: impl FnMut(A) -> T + 'static) -> Self {
        Self::stream(stream.map(map))
    }
    pub fn perform<A: 'static>(future: impl Future<Output = A> + 'static, map: impl Fn(A) -> T + 'static) -> Self {
        Self::future(async move { map(future.await) })
    }
    pub fn batch(tasks: impl IntoIterator<Item = Self>) -> Self {
        let streams: Vec<_> = tasks.into_iter().filter_map(|task| task.0).collect();
        if streams.is_empty() { return Self::none(); }
        Self::stream(futures::stream::select_all(streams))
    }
    pub fn map<U: 'static>(self, map: impl FnMut(T) -> U + 'static) -> Task<U> {
        Task(self.0.map(|stream| stream.map(map).boxed_local()))
    }
    pub fn then<U: 'static>(self, mut next: impl FnMut(T) -> Task<U> + 'static) -> Task<U> {
        Task(self.0.map(|stream| stream.flat_map(move |value| next(value).into_stream()).boxed_local()))
    }
    pub fn chain(self, next: Self) -> Self {
        Self::stream(self.into_stream().chain(next.into_stream()))
    }
    pub fn discard<U: 'static>(self) -> Task<U> {
        Task(self.0.map(|stream| stream.filter_map(|_| std::future::ready(None)).boxed_local()))
    }
    pub fn into_stream(self) -> BoxStream<T> {
        self.0.unwrap_or_else(|| futures::stream::empty().boxed_local())
    }
    pub fn abortable(self) -> (Self, Handle) {
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        let task = Self::stream(futures::stream::Abortable::new(self.into_stream(), registration));
        (task, Handle { abort, guard: None })
    }
}

impl<T: 'static> Task<Option<T>> {
    pub fn and_then<U: 'static>(self, mut next: impl FnMut(T) -> Task<U> + 'static) -> Task<U> {
        self.then(move |value| value.map_or_else(Task::none, &mut next))
    }
}

pub struct Handle {
    abort: futures::future::AbortHandle,
    guard: Option<futures::future::AbortHandle>,
}
impl Handle {
    pub fn abort(&self) { self.abort.abort(); }
    pub fn abort_on_drop(mut self) -> Self { self.guard = Some(self.abort.clone()); self }
}
impl Drop for Handle {
    fn drop(&mut self) { if let Some(abort) = &self.guard { abort.abort(); } }
}
