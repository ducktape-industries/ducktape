//! Draft and upload decisions belong to the conversation guest.
use super::{ChatView, Message, MessageAction};
use ducktape_view_composer::host::{self, SelectedFile, Target};
use ducktape_view_composer::{self as composer, Attachment, AttachmentState, Event, Outcome, Send};
use ducktape_view_guest::{Task, widget, wire};

#[derive(Clone, Hash)]
pub struct Route {
    scope: String,
    key: String,
    target: Target,
    connection: i64,
}
#[derive(Clone)]
pub enum ComposerMessage {
    Editor(Route, Box<Event<Message>>),
    Picked(Route, Result<Vec<SelectedFile>, String>),
    Uploaded(Route, String, Result<String, String>),
    Clipboard(Route, Result<host::Clipboard, String>),
    Prepared(Route, Send, Result<String, String>),
    Sent(Route, String, Send, Result<(), String>),
    Copied(Route, Result<(), String>),
}
impl ChatView {
    pub(crate) fn composer_drops(&self) -> ducktape_view_guest::Subscription<Message> {
        use futures::StreamExt;
        let accepts_files =
            self.connected && !self.active_channel.is_empty() && self.post_refusal.is_empty();
        if !accepts_files {
            return ducktape_view_guest::Subscription::none();
        }
        let thread = (self.active_thread_seq > 0).then_some(self.active_thread_seq as u64);
        let scope = match thread {
            Some(thread) => {
                crate::host::thread_scope(&self.endpoint, &self.active_channel, thread as i64)
            }
            None => crate::host::composer_scope(&self.endpoint, &self.active_channel),
        };
        let route = Route {
            scope,
            key: String::new(),
            target: Target::Post {
                channel: self.active_channel.clone(),
                thread,
            },
            connection: self.connection_serial,
        };
        ducktape_view_guest::Subscription::run_with(route, |route| {
            let route = route.clone();
            ducktape_view_guest::host::subscribe("fs.drops", b"{}").map(move |answer| {
                let result = answer.and_then(|bytes| {
                    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
                });
                Message::Composer(Box::new(ComposerMessage::Picked(route.clone(), result)))
            })
        })
    }
    pub(crate) fn composer(
        &self,
        key: String,
        scope: String,
        target: Target,
        hint: &str,
        editable: bool,
    ) -> wire::Node {
        let draft = self.composers.get(&scope).cloned().unwrap_or_default();
        let choices = crate::host::composer_choices(&self.channel_members);
        let route = Route {
            scope,
            key: key.clone(),
            target,
            connection: self.connection_serial,
        };
        composer::view(&draft, &key, hint, editable, &choices, move |event| {
            Message::Composer(Box::new(ComposerMessage::Editor(
                route.clone(),
                Box::new(event),
            )))
        })
    }
    pub(crate) fn on_composer(&mut self, message: ComposerMessage) -> Task<Message> {
        match message {
            ComposerMessage::Editor(route, event) => self.composer_editor(route, *event),
            ComposerMessage::Picked(route, result) => self.composer_picked(route, result),
            ComposerMessage::Uploaded(route, token, result) => {
                self.composer_uploaded(route, token, result)
            }
            ComposerMessage::Clipboard(route, result) => self.composer_clipboard(route, result),
            ComposerMessage::Prepared(route, send, result) => {
                self.composer_prepared(route, send, result)
            }
            ComposerMessage::Sent(route, id, send, result) => {
                self.composer_sent(route, id, send, result)
            }
            ComposerMessage::Copied(route, result) => self.composer_copied(route, result),
        }
    }
    fn composer_editor(&mut self, route: Route, event: Event<Message>) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        let choices = crate::host::composer_choices(&self.channel_members);
        let draft = self.composers.entry(route.scope.clone()).or_default();
        match draft.handle(event, &choices) {
            Outcome::Updated => Task::none(),
            Outcome::Message(message) => self.update(message),
            Outcome::Enqueue(tag) => widget::perform(wire::WidgetCommand::EditorAction {
                target: format!("{}/editor", route.key),
                tag,
            }),
            Outcome::Action(tag) => self.composer_action(route, &tag),
        }
    }
    fn composer_action(&mut self, route: Route, tag: &str) -> Task<Message> {
        if let Some(token) = tag.strip_prefix("remove:") {
            if let Some(handle) = self.upload_handles.remove(token) {
                handle.abort();
            }
            self.composers
                .entry(route.scope.clone())
                .or_default()
                .attachments
                .retain(|file| file.token != token);
            let token = token.to_owned();
            return Task::perform(
                async move {
                    host::release(&token).await;
                },
                move |()| {
                    Message::Composer(Box::new(ComposerMessage::Copied(route.clone(), Ok(()))))
                },
            );
        }
        if let Some(token) = tag.strip_prefix("retry:") {
            let draft = self.composers.entry(route.scope.clone()).or_default();
            let Some(index) = draft.attachments.iter().position(|file| {
                file.token == token && matches!(file.state, AttachmentState::Failed { .. })
            }) else {
                return Task::none();
            };
            let file = draft.attachments.remove(index);
            return self.composer_picked(
                route,
                Ok(vec![SelectedFile {
                    token: file.token,
                    name: file.name,
                    bytes: file.bytes,
                }]),
            );
        }
        match tag {
            "send" => self.composer_send(route),
            "attach" => {
                if matches!(route.target, Target::Edit { .. }) {
                    return Task::none();
                }
                Task::perform(host::pick(), move |result| {
                    Message::Composer(Box::new(ComposerMessage::Picked(route.clone(), result)))
                })
            }
            "paste" => Task::perform(host::clipboard(), move |result| {
                Message::Composer(Box::new(ComposerMessage::Clipboard(route.clone(), result)))
            }),
            "copy" | "cut" => {
                let Some(text) = self
                    .composers
                    .entry(route.scope.clone())
                    .or_default()
                    .clipboard
                    .take()
                else {
                    return Task::none();
                };
                Task::future(async move {
                    let result = host::copy(&text).await;
                    Message::Composer(Box::new(ComposerMessage::Copied(route, result)))
                })
            }
            _ => Task::none(),
        }
    }
    fn composer_send(&mut self, route: Route) -> Task<Message> {
        let draft = self.composers.entry(route.scope.clone()).or_default();
        let Some(send) = draft.submitted.take() else {
            return Task::none();
        };
        draft.in_flight.push(send.clone());
        Task::future(async move {
            let result = host::id().await;
            Message::Composer(Box::new(ComposerMessage::Prepared(route, send, result)))
        })
    }
    fn composer_prepared(
        &mut self,
        route: Route,
        send: Send,
        result: Result<String, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        let id = match result {
            Ok(id) => id,
            Err(error) => return self.composer_sent(route, String::new(), send, Err(error)),
        };
        if let Target::Post { channel, thread } = &route.target {
            self.sending.insert(
                id.clone(),
                (
                    channel.clone(),
                    crate::host::PendingSend {
                        id: id.clone(),
                        body: host::body(&send),
                        thread_seq: thread.unwrap_or_default() as i64,
                    },
                ),
            );
            self.refresh_pending();
        }
        Task::future(async move {
            let result = host::submit(id.clone(), &send, &route.target).await;
            Message::Composer(Box::new(ComposerMessage::Sent(route, id, send, result)))
        })
    }
    pub(crate) fn refresh_pending(&mut self) {
        self.pending_sends = self
            .sending
            .values()
            .filter(|(channel, _)| channel == &self.active_channel)
            .map(|(_, send)| send.clone())
            .collect();
        self.messages =
            crate::host::with_pending(&self.room_messages, &self.pending_sends, 0, &self.me);
        let committed = self
            .thread_messages
            .iter()
            .filter(|row| !row.pending)
            .cloned()
            .collect::<Vec<_>>();
        self.thread_messages = crate::host::with_pending(
            &committed,
            &self.pending_sends,
            self.active_thread_seq,
            &self.me,
        );
    }

    fn composer_picked(
        &mut self,
        route: Route,
        result: Result<Vec<SelectedFile>, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        let files = match result {
            Ok(files) => files,
            Err(error) => {
                self.composers.entry(route.scope).or_default().note = error;
                return Task::none();
            }
        };
        let draft = self.composers.entry(route.scope.clone()).or_default();
        let mut tasks = Vec::new();
        for file in files {
            draft.attachments.push(Attachment {
                token: file.token.clone(),
                name: file.name.clone(),
                bytes: file.bytes,
                state: AttachmentState::Uploading,
            });
            let route = route.clone();
            let upload_token = file.token.clone();
            let (task, handle) = Task::future(async move {
                let token = file.token.clone();
                let result = host::upload(file).await;
                Message::Composer(Box::new(ComposerMessage::Uploaded(route, token, result)))
            })
            .abortable();
            self.upload_handles
                .insert(upload_token, handle.abort_on_drop());
            tasks.push(task);
        }
        Task::batch(tasks)
    }
    fn composer_uploaded(
        &mut self,
        route: Route,
        token: String,
        result: Result<String, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        self.upload_handles.remove(&token);
        let Some(draft) = self.composers.get_mut(&route.scope) else {
            return Task::none();
        };
        if let Some(file) = draft
            .attachments
            .iter_mut()
            .find(|file| file.token == token)
        {
            file.state = match result {
                Ok(uri) => AttachmentState::Ready { uri },
                Err(reason) => AttachmentState::Failed { reason },
            };
        }
        Task::none()
    }
    fn composer_clipboard(
        &mut self,
        route: Route,
        result: Result<host::Clipboard, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        let clipboard = match result {
            Ok(clipboard) => clipboard,
            Err(error) => {
                self.composers.entry(route.scope).or_default().note = error;
                return Task::none();
            }
        };
        self.composers.entry(route.scope.clone()).or_default().paste = Some(clipboard.text);
        let paste = widget::perform(wire::WidgetCommand::EditorAction {
            target: format!("{}/editor", route.key),
            tag: "paste-ready".into(),
        });
        let files = match route.target {
            Target::Post { .. } => self.composer_picked(route, Ok(clipboard.files)),
            Target::Edit { .. } => Task::none(),
        };
        Task::batch([paste, files])
    }
    fn composer_copied(&mut self, route: Route, result: Result<(), String>) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        if let Err(error) = result {
            self.composers.entry(route.scope).or_default().note = error;
        }
        Task::none()
    }
    fn composer_sent(
        &mut self,
        route: Route,
        id: String,
        send: Send,
        result: Result<(), String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        if let Some(draft) = self.composers.get_mut(&route.scope) {
            draft.complete_send(&send);
        }
        match result {
            Ok(()) => {
                self.room_serial += 1;
                if let Target::Edit { seq, .. } = route.target {
                    if self.selected_message_seq == seq as i64 {
                        self.message_action = MessageAction::Toolbar;
                    }
                    if self.thread_selected_seq == seq as i64 {
                        self.thread_message_action = MessageAction::Toolbar;
                    }
                }
            }
            Err(error) => {
                self.sending.remove(&id);
                self.refresh_pending();
                let draft = self.composers.entry(route.scope).or_default();
                draft.failed(send);
                draft.note = error;
            }
        }
        Task::none()
    }
}
